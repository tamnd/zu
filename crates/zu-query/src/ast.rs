//! Abstract syntax for the zuQL core (docs/07 §1, docs/grammar.ebnf).
//!
//! The parser produces this shape verbatim; name resolution, typing,
//! and structural rules that need the catalog (which variables exist,
//! what a label means) belong to the binder, so the AST stays a plain
//! description of the text.

use zu_common::unicode::NormalForm;
use zu_common::{DurationKind, LogicalType, Temporal};

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
    /// GP18. Several of them chained by `NEXT`, at least one of which
    /// changes the catalog and at least one of which does not.
    ///
    /// A part is carried as its own text rather than as its tree. Every
    /// part is a statement whose runner already exists, plan cache and
    /// write splitting and all, and the text is what keys that cache,
    /// so handing the runner the text is handing it the thing it takes.
    /// The parts have all been parsed by the time this is built, so a
    /// block holding a part that does not parse is a syntax error
    /// before any of it runs.
    Block(Vec<String>),
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

/// A procedure's name and the schema it is in, named the way a graph is
/// named because a procedure is a catalog object the same way a graph
/// is. A reference with no path in it is resolved in the schema the
/// call says it is at, and in the root schema when it says nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcRef {
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
    /// GP16, the `AT` schema clause: the schema a name written without
    /// a path in it is resolved in. `None` is a query with no `AT`,
    /// which resolves in the schema the session is working in.
    pub at_schema: Option<SchemaRef>,
    /// GQ01: the graph the statements below run against, written as a
    /// `USE` clause in front of them. `None` is a query with no `USE`,
    /// which runs against whatever graph the session is working in.
    pub use_graph: Option<GraphRef>,
    /// GP17, the binding variable definition block: the names the
    /// statements below may read that no row carries. Empty for every
    /// query written without one, which is most of them.
    pub bindings: Vec<BindingDef>,
    pub body: Composite,
}

/// What a binding variable definition defines (ISO 13.3), which
/// decides what the name may stand for and where it may be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    /// `VALUE v = ...`, GP05 through GP07. A value of any type a
    /// column can hold, which is every type except the two below.
    Value,
    /// `BINDING TABLE t = ...`, GP08 through GP10. The rows of a
    /// result, held once and read by whatever runs over them.
    Table,
    /// `GRAPH g = ...`, GP11 through GP13. A reference to a graph,
    /// which is what a `USE` names and what a graph valued expression
    /// answers.
    Graph,
}

impl BindingKind {
    /// How the kind reads in a diagnostic, which is how it is written.
    pub fn word(self) -> &'static str {
        match self {
            BindingKind::Value => "VALUE",
            BindingKind::Table => "BINDING TABLE",
            BindingKind::Graph => "GRAPH",
        }
    }
}

/// One binding variable definition (ISO 13.3, GP05 through GP13 and
/// GP17): a name, what kind of thing it stands for, and what it is.
///
/// A definition is worked out once, where it is written, and not once
/// for each place the name is read. That is the difference between
/// this and a `LET`, and it is why the initializer may be a whole
/// query without the query becoming a correlated subquery: there is no
/// row here for it to be correlated to.
#[derive(Debug, Clone, PartialEq)]
pub struct BindingDef {
    pub kind: BindingKind,
    pub name: String,
    /// The type written between the name and the `=`, `None` when the
    /// definition leaves the type to whatever it initializes with.
    pub ty: Option<LogicalType>,
    pub init: BindingInit,
}

/// What a binding variable is initialized with, which is either an
/// expression or a query.
#[derive(Debug, Clone, PartialEq)]
pub enum BindingInit {
    /// `= 1 + 1`, `= $t`, `= HOME_PROPERTY_GRAPH`. An expression, which
    /// covers the reference forms of all three kinds since a graph
    /// reference and a binding table reference are both expressions.
    Expr(Expr),
    /// `= { MATCH (p:Person) RETURN p.name AS name }`. A query, which
    /// answers one value for a `VALUE` and its whole result for a
    /// `BINDING TABLE`.
    Query(Box<Query>),
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
                for clause in &simple.clauses {
                    out.push(clause);
                    // The block of an inline call is part of the text
                    // and its statements are statements of the query,
                    // so a reader asking what the query names has to be
                    // told about them. An INSERT inside a block writes
                    // the same graph an INSERT beside it writes.
                    if let Clause::CallInline { body, .. } = clause {
                        out.extend(body.clauses());
                    }
                }
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

/// What an `AT` schema clause names (ISO 16.1, `at schema clause`),
/// which is the schema a reference written without a path in it is
/// resolved in.
///
/// A schema here is a directory of the catalog and not a graph type,
/// so what the clause changes is where a name is looked up, not what
/// the statement reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaRef {
    /// `AT CURRENT_SCHEMA`, the schema the session is resolving names
    /// in already, which is what the clause names when it names no
    /// path at all.
    Current,
    /// `AT HOME_SCHEMA`, the schema the session started in.
    Home,
    /// A schema by its path, `AT /app`, which is absolute, and the
    /// root schema is written `/`.
    Path(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    Match {
        optional: bool,
        patterns: Vec<PathPattern>,
        /// The alternatives written after the first one, ISO 16.7 and
        /// features G030 and G032, each of them a whole pattern list of
        /// its own. Empty for a match with no bar in it, which is every
        /// match that describes one shape.
        ///
        /// A bar stands between two path patterns and a comma stands
        /// between two pattern lists, so `A | B, C` describes two ways
        /// of matching and each of them is a list: the parser writes
        /// out the lists rather than keeping the bar, because what the
        /// clause answers is the rows of one list or the rows of the
        /// other and there is no operator between them for anything
        /// downstream to read. `patterns` is the first of the lists and
        /// this is the rest, so a reader that does not know about
        /// alternation sees the shape that was written first.
        alts: Vec<Vec<PathPattern>>,
        /// Whether the alternatives are a set or a bag: `|` is the path
        /// pattern union of feature G032, which answers each path once,
        /// and `|+|` is the multiset alternation of feature G030, which
        /// answers a path as many times as the alternatives matched it.
        /// False for a match with no bar in it, where there is one
        /// alternative and nothing to add up.
        distinct: bool,
        filter: Option<Expr>,
    },
    /// `INSERT (x:Person {name: 'Zoe'}), (y:Person)`, the statement
    /// that adds elements (ISO 13.2). The patterns are the same shape a
    /// MATCH writes, because GQL writes them the same way: what makes
    /// this one a write is the clause in front of them, and a pattern
    /// here is a description of an element to create rather than one to
    /// look for.
    Insert { patterns: Vec<PathPattern> },
    /// `MERGE (p:person {id: 7})`, the statement that finds a pattern
    /// or writes it (Cypher; GQL has no word for it). Exactly one
    /// pattern, because what the statement does with two of them, find
    /// one and write the other, is not a thing it could mean.
    ///
    /// `ON CREATE SET` runs on the rows the pattern was written for and
    /// `ON MATCH SET` on the rows it was found for, so between them
    /// every row the statement ran for is covered once. Either may be
    /// left out and neither may be written twice.
    Merge {
        pattern: PathPattern,
        on_create: Vec<SetItem>,
        on_match: Vec<SetItem>,
    },
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
    /// The source may also be a binding table (GQ23), where the rows
    /// of the table are what the statement runs over and each of them
    /// arrives as a record over the table's columns. It is the same
    /// statement either way: a table is a sequence of rows the way a
    /// list is a sequence of elements, so nothing about the counter or
    /// the binding changes with it.
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
    /// `CALL [AT /schema] name(args) YIELD col [AS alias], ...`, the
    /// named procedure call of ISO 13.1 and feature GP04.
    ///
    /// The name is a catalog object reference and not a word, so it may
    /// be written in full as a path and it is resolved in a schema.
    Call {
        /// The `AT` clause, which says the schema the reference is
        /// resolved in when the reference does not say it itself.
        at: Option<String>,
        proc: ProcRef,
        args: Vec<Expr>,
        /// `(column, alias)` pairs; the column names are fixed by the
        /// function, the alias is what later clauses see.
        yields: Vec<(String, Option<String>)>,
    },
    /// `CALL { MATCH (a)-[:knows]->(f) RETURN f AS friend }`, the
    /// inline procedure call of ISO 13.2 and features GP01, GP02 and
    /// GP03.
    ///
    /// The block is a statement of its own written inside another one.
    /// It runs once for each row that reaches it, the names it is
    /// allowed to read tie it to that row, and what its `RETURN` names
    /// is added to the row rather than put in place of it. That last
    /// part is what makes it a `CALL` and not a `WITH`: the row goes on
    /// carrying everything it carried, wider by what the block
    /// answered.
    ///
    /// `scope` is the variable scope clause, which says what the block
    /// may read of the row. `None` is a block written with no clause at
    /// all, which reads everything (GP02), and `Some` is the list the
    /// reader wrote (GP03). An empty list is a block that reads nothing
    /// of the row, which is a whole statement standing on its own, and
    /// it is written `CALL () { ... }`.
    ///
    /// `optional` is the `OPTIONAL` in front of the word (GP03). A block
    /// written with it keeps the row when it answers no rows at all, and
    /// every name it lets out is null on that row, which is what the
    /// same word says in front of a `MATCH`. A block that answers rows
    /// is the call it always was.
    CallInline {
        optional: bool,
        scope: Option<Vec<String>>,
        body: Box<Query>,
    },
    With {
        projection: Projection,
        filter: Option<Expr>,
    },
    /// `ORDER BY p.age SKIP 1 LIMIT 3` standing where a statement
    /// stands, the order by and page statement of ISO 14.9.
    ///
    /// It is the tail of a projection with no projection in front of
    /// it, and it says what a reader means: sort what is in hand, then
    /// take a page of it, and leave the columns alone. The same words
    /// written after a `RETURN` belong to that projection and are on
    /// [`Projection`], because there they say what the answer looks
    /// like; here they say what the rows going into the next statement
    /// are.
    ///
    /// All three parts are optional and at least one of them was
    /// written, since `ORDER BY` alone, `SKIP` alone and `LIMIT` alone
    /// are each a whole statement in the standard's grammar.
    Order {
        keys: Vec<SortKey<Expr>>,
        skip: Option<Expr>,
        limit: Option<Expr>,
    },
    /// `FINISH`, the primitive result statement of ISO 14.10 that
    /// says the query has no result.
    ///
    /// It is the last clause of the last statement, which the parser
    /// makes sure of: nothing may follow it and nothing may read from
    /// it. It is a clause here rather than another kind of result
    /// because what it does is end the statement, and the result it
    /// leaves is a table with no columns and no rows.
    Finish,
}

/// One item of a `DELETE`, which ISO writes as a value expression and
/// splits into two optional features: a simple expression naming an
/// element (GD04) and a subquery answering one (GD03).
#[derive(Debug, Clone, PartialEq)]
pub enum DeleteTarget {
    /// `DELETE n`: a variable an earlier clause bound.
    Variable(String),
    /// `DELETE VALUE { MATCH (p:Person) WHERE p.name = 'Ada' RETURN p }`,
    /// the value query expression of ISO 20.6. The query inside runs on
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

/// GQL match mode (ISO 16.9, features G002 and G003): what a list of
/// path patterns is allowed to bind twice.
///
/// A path mode speaks about one path and this speaks about the list of
/// them, which is why the two are separate words in the standard and
/// separate types here. The mode is written once in front of the list
/// and it settles two things at once: the path mode a pattern that
/// named none walks under, and whether the patterns of the list may
/// share an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchMode {
    /// `DIFFERENT EDGES`, which is what a list that names no mode
    /// means: no edge of the graph answers two of the edge patterns of
    /// the list at once, and each path is a trail.
    #[default]
    DifferentEdges,
    /// `REPEATABLE ELEMENTS`: an edge may answer as many of the edge
    /// patterns as it fits, and a path under it is a walk.
    RepeatableElements,
}

impl MatchMode {
    /// The path mode a pattern of this list walks under when it names
    /// none of its own.
    ///
    /// `DIFFERENT EDGES` says no path repeats an edge, which is what
    /// `TRAIL` says, and `REPEATABLE ELEMENTS` lifts that, which leaves
    /// `WALK`. The rule that an unbounded walk needs a selector still
    /// holds under it, so `REPEATABLE ELEMENTS` on an unbounded pattern
    /// is refused rather than run forever.
    pub fn path_mode(self) -> PathMode {
        match self {
            MatchMode::DifferentEdges => PathMode::Trail,
            MatchMode::RepeatableElements => PathMode::Walk,
        }
    }
}

/// Which list of path patterns a pattern was written in, and under
/// which match mode (ISO 16.9).
///
/// Both halves are said once for a whole list, and both are carried per
/// pattern because a match statement block gathers the lists of several
/// statements into one clause. The number is what tells those lists
/// apart afterwards: `DIFFERENT EDGES` speaks about the patterns of one
/// list, so two patterns of two statements may bind the same edge even
/// though two of one statement may not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PatternList {
    pub mode: MatchMode,
    /// Which list, counted as the statement was read.
    pub at: u32,
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

/// A parenthesized path pattern (ISO 16.11, feature G038): a stretch of
/// a path written inside brackets, and what the brackets let it carry.
///
/// The brackets themselves match nothing. A path written with them and
/// the same path written without them walk the same edges in the same
/// order, which is why this is a note about a stretch of a path rather
/// than a shape of its own: `from` and `to` are node positions of the
/// path the brackets sit in, counting the first node as zero, and the
/// steps between them are the ones the brackets hold.
///
/// What the brackets are for is the three things written inside them. A
/// subpath variable names the stretch, so `((a)-[e]->(b))` is a path
/// value over two nodes and one edge where the pattern around it may be
/// longer. A path mode applies to the stretch alone, so an outer walk
/// may hold an inner trail. A `WHERE` inside the brackets is a condition
/// on the stretch, and it may read a variable the pattern bound outside
/// it, which is what makes it a non local predicate rather than a
/// condition on one element.
#[derive(Debug, Clone, PartialEq)]
pub struct Subpath {
    /// `(p = ...)`, the name the stretch is bound to, `None` for
    /// brackets written to carry a mode or a condition.
    pub var: Option<String>,
    /// A path mode written inside the brackets, which the steps between
    /// `from` and `to` walk under whatever the pattern around them
    /// walks under.
    pub mode: Option<PathMode>,
    /// The node position the stretch starts at, counting the first node
    /// of the pattern as zero.
    pub from: usize,
    /// The node position it ends at. `from == to` is a stretch of one
    /// node and no edge, which is what a bracket around a single node
    /// pattern is.
    pub to: usize,
}

/// A name a quantified stretch bound more than once (ISO 16.11, feature
/// GQ17): the group variable, and where in the pattern its bindings are.
///
/// A stretch repeated n times binds every name inside it n times, once
/// per repetition, so the name does not stand for one element the way an
/// ordinary pattern variable does. It stands for all of them, in the
/// order the walk took them, and that is a list. `at` holds the
/// positions of the bindings in the flattened pattern, which the binder
/// turns into slots: the group is read out of the row the walk already
/// filled rather than gathered into a place of its own, so a query that
/// does not read the group costs nothing for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub name: String,
    /// Whether the positions are node positions or step positions,
    /// which is what says whether the group is a list of nodes or a
    /// list of edges.
    pub kind: GroupKind,
    /// The positions the name was bound at, in written order. Node
    /// positions count the first node of the pattern as zero, and step
    /// positions count the first edge as zero.
    pub at: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKind {
    Node,
    Rel,
}

/// A stretch of a pattern that a quantifier repeated (ISO 16.11,
/// feature G035), as the step positions the repetitions occupy.
///
/// The repetitions are written out into the one linear pattern, so
/// nothing downstream can tell that two steps came from one step
/// repeated. Edge distinctness is where that matters: two copies of one
/// step answer the same edge whenever the graph holds a loop where the
/// stretch begins, and a quantified stretch walks a trail by default, so
/// the copies have to be kept apart. This records which steps are the
/// copies, and the binder writes the test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Repeat {
    /// The step position the first repetition starts at, counting the
    /// first edge of the pattern as zero.
    pub from: usize,
    /// One past the step position the last repetition ends at.
    pub to: usize,
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
    /// The list this pattern was written in, which says the match mode
    /// it walks under and which of a block's lists it belongs to. A
    /// pattern written where no list is read, an `INSERT` pattern, takes
    /// the default and nothing asks it anything.
    pub list: PatternList,
    pub start: NodePattern,
    pub steps: Vec<(RelPattern, NodePattern)>,
    /// The parenthesized stretches of this pattern, in the order their
    /// brackets closed, so a bracket inside another comes first. Each
    /// points at the node positions of `start` and `steps` it covers.
    /// Empty for a pattern written with no brackets at all, which is
    /// every pattern that was legal before G038.
    pub subpaths: Vec<Subpath>,
    /// The names a quantified stretch of this pattern bound more than
    /// once, in the order they were written. Empty for a pattern with no
    /// quantified stretch in it, which is every pattern whose names each
    /// stand for one element.
    pub groups: Vec<Group>,
    /// The stretches a quantifier repeated, in the order the brackets
    /// closed. Empty for a pattern with no quantifier on brackets in it.
    pub repeats: Vec<Repeat>,
    /// The conditions written inside those brackets, folded together
    /// with AND.
    ///
    /// It is kept on the pattern rather than folded into the clause's
    /// own `WHERE` because the two are written in different places and a
    /// reader of an error should be told which one they wrote. What it
    /// means is the same thing: a condition inside brackets decides the
    /// match, the way an `OPTIONAL MATCH`'s `WHERE` does, so it is bound
    /// with the clause condition and not behind it.
    pub filter: Option<Expr>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodePattern {
    pub var: Option<String>,
    /// The other names this node was written under, which is what two
    /// stretches of a pattern meeting at it leaves behind (ISO 16.11).
    ///
    /// `(a:Step) ((x:Step)-[:LINK]->(y:Step))` describes two nodes and
    /// not four: the `a` and the `x` are one node the two stretches each
    /// named, so one of the names is the pattern's and the rest are
    /// here, and all of them stand for the one element for the rest of
    /// the query. Empty for every pattern written without brackets, and
    /// for the ones whose stretches met under one name.
    pub aliases: Vec<String>,
    /// The label expression after the colon, `None` for a pattern that
    /// names no label and therefore matches whatever it reaches.
    pub label: Option<LabelExpr>,
    pub props: Vec<(String, Expr)>,
    /// The `WHERE` written inside the parentheses, ISO 16.6 and feature
    /// G041, `None` for a pattern that wrote none.
    ///
    /// It is the element pattern predicate, asked of the one node the
    /// pattern is standing on rather than of the row a whole pattern
    /// built, so `(a)-[:LINK]->(b WHERE b.step > a.step)` reads the node
    /// it has just reached and the node it came from. Being part of the
    /// pattern is what makes it different from the same text behind the
    /// pattern under an `OPTIONAL MATCH`, where a condition that fails
    /// leaves the row with nulls rather than dropping it.
    pub filter: Option<Box<Expr>>,
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

/// Which end of a string an explicit `TRIM` takes characters off, ISO
/// 20.24's trim specification. The three words are the whole of the
/// production, and leaving them out means BOTH.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimSide {
    Leading,
    Trailing,
    Both,
}

/// The temporal value functions of ISO 20.27 and 20.29: the words a
/// statement asks the time with, the constructors that read one out of
/// a string, and the duration function beside them.
///
/// The three CURRENT words answer in the session's displacement and the
/// two LOCAL ones answer without a displacement at all, which is the
/// whole of what separates them. All of them are cut from one instant,
/// so a statement holding CURRENT_DATE and CURRENT_TIMESTAMP cannot
/// have them land on two different days.
///
/// The constructors are the same functions with a string in front of
/// the instant: `DATE('2024-01-15')` reads the string and `DATE()`
/// reads the clock, and one kernel answers both because the only
/// difference is where the value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalFn {
    /// The date in the session's displacement.
    CurrentDate,
    /// The time of day in it, carrying the displacement.
    CurrentTime,
    /// The instant, carrying the displacement.
    CurrentTimestamp,
    /// The time of day with no displacement on it. The one word of the
    /// nine that is written both bare and with brackets, which is what
    /// the standard's `LOCAL_TIME [ ( ... ) ]` says.
    LocalTime,
    /// The date and time of day with no displacement on it.
    LocalTimestamp,
    /// `DATE('2024-01-15')` or `DATE()`.
    Date,
    /// `ZONED_TIME('10:00:00+07:00')` or `ZONED_TIME()`.
    ZonedTime,
    /// `ZONED_DATETIME('2024-01-15T10:00:00Z')` or `ZONED_DATETIME()`.
    ZonedDatetime,
    /// `LOCAL_DATETIME('2024-01-15T10:00:00')` or `LOCAL_DATETIME()`.
    LocalDatetime,
    /// `DURATION('P1Y2M')`, ISO 20.29. The one of these with no form
    /// that reads the clock: a length of time is not a thing the
    /// present moment is.
    Duration,
}

/// Where a temporal function may write its brackets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Brackets {
    /// Never, which is the five words of ISO 20.27 that are written
    /// bare. `CURRENT_DATE()` is a call to a function nobody defined.
    Never,
    /// Always, and a value inside them or nothing.
    Always,
    /// Either, which is `LOCAL_TIME` alone.
    Optional,
}

impl TemporalFn {
    /// The word a statement writes for it, which is also the name the
    /// registry row carries and the plan prints.
    pub fn word(self) -> &'static str {
        match self {
            TemporalFn::CurrentDate => "current_date",
            TemporalFn::CurrentTime => "current_time",
            TemporalFn::CurrentTimestamp => "current_timestamp",
            TemporalFn::LocalTime => "local_time",
            TemporalFn::LocalTimestamp => "local_timestamp",
            TemporalFn::Date => "date",
            TemporalFn::ZonedTime => "zoned_time",
            TemporalFn::ZonedDatetime => "zoned_datetime",
            TemporalFn::LocalDatetime => "local_datetime",
            TemporalFn::Duration => "duration",
        }
    }

    /// The temporal type the word answers, which is also the cut of the
    /// instant it is and the type a string in front of it is read as.
    /// Every one of these is a conversion the cast rules already state,
    /// so they are ten targets and one piece of code rather than ten
    /// pieces of arithmetic.
    ///
    /// The duration target names the day-time kind and is not one: a
    /// duration string says which kind it is, `P1Y` being months and
    /// `P1D` nanoseconds, so the target here means read this as a
    /// duration and the string settles the rest.
    pub fn target(self) -> LogicalType {
        match self {
            TemporalFn::CurrentDate | TemporalFn::Date => LogicalType::Date,
            TemporalFn::CurrentTime | TemporalFn::ZonedTime => LogicalType::ZonedTime,
            TemporalFn::CurrentTimestamp | TemporalFn::ZonedDatetime => LogicalType::ZonedDatetime,
            TemporalFn::LocalTime => LogicalType::LocalTime,
            TemporalFn::LocalTimestamp | TemporalFn::LocalDatetime => LogicalType::LocalDatetime,
            TemporalFn::Duration => LogicalType::Duration(DurationKind::DayTime),
        }
    }

    /// Where this one's brackets may stand, which is what the parser
    /// reads it by and the whole of what separates `CURRENT_DATE` from
    /// `DATE(...)`.
    pub fn brackets(self) -> Brackets {
        match self {
            TemporalFn::CurrentDate
            | TemporalFn::CurrentTime
            | TemporalFn::CurrentTimestamp
            | TemporalFn::LocalTimestamp => Brackets::Never,
            TemporalFn::LocalTime => Brackets::Optional,
            TemporalFn::Date
            | TemporalFn::ZonedTime
            | TemporalFn::ZonedDatetime
            | TemporalFn::LocalDatetime
            | TemporalFn::Duration => Brackets::Always,
        }
    }

    /// Whether the brackets may stand empty, which is every one of them
    /// but the duration: there is a date now and a time now, and there
    /// is no length of time now.
    pub fn reads_the_clock(self) -> bool {
        self != TemporalFn::Duration
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
    /// `NORMALIZE(s, NFC)`, ISO 20.24. Written like a call and not one,
    /// for the reason CAST is not one: the second argument names a
    /// normal form, and reading it as an expression would bind a
    /// variable called NFC. The form defaults to NFC, which is what the
    /// standard defaults it to.
    Normalize {
        expr: Box<Expr>,
        form: NormalForm,
    },
    /// `TRIM(LEADING 'x' FROM s)`, ISO 20.24, GF06. Written like a call
    /// and not one: the first word names an end of the string rather
    /// than a value, and FROM is not a separator a call has. The one
    /// argument form and the multi-character functions are ordinary
    /// calls and do not come through here.
    Trim {
        side: TrimSide,
        /// The character to trim, or none, which means a space.
        chars: Option<Box<Expr>>,
        source: Box<Expr>,
    },
    /// The temporal value functions of ISO 20.27 and 20.29. They are
    /// here rather than resolved by name because where the brackets may
    /// stand is part of each one: `CURRENT_DATE` takes none,
    /// `DATE(...)` takes them, and `LOCAL_TIME` takes them or not. A
    /// name resolved against the registry could not tell those apart.
    Temporal {
        func: TemporalFn,
        /// The string the value is read out of, or none, which means
        /// read the clock.
        arg: Option<Box<Expr>>,
    },
    /// The instant the statement is running at, which no query writes.
    /// It is the argument the binder hands each of the words above that
    /// wrote no string of its own, so that the clock is read in one
    /// place and cut in as many as ask for it, and so that every one of
    /// them in a statement answers the same instant.
    Clock,
    /// `DURATION_BETWEEN(a, b) [YEAR TO MONTH | DAY TO SECOND]`, ISO
    /// 20.28's datetime subtraction. Written like a call and not one,
    /// because the qualifier stands behind the closing parenthesis
    /// where no call has anything, and the two words it is made of name
    /// fields rather than values.
    ///
    /// The arguments are held as written rather than as a pair, so a
    /// call with the wrong number of them is refused where every other
    /// call is, by the arity on its row.
    DurationBetween {
        args: Vec<Expr>,
        /// The kind the qualifier asked for, or none when none was
        /// written, which reads as DAY TO SECOND.
        kind: Option<DurationKind>,
    },
    /// `expr IS [NOT] NORMALIZED [NFC]`, ISO 19.7. The same question the
    /// function answers, asked as a predicate, and the form defaults the
    /// same way.
    IsNormalized {
        expr: Box<Expr>,
        form: NormalForm,
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
    /// `expr IS [NOT] DIRECTED`, ISO 19.8 and G110. The question is
    /// asked of an edge and of nothing else, because a node has no
    /// direction to answer with.
    IsDirected {
        expr: Box<Expr>,
        negated: bool,
    },
    /// `expr IS [NOT] LABELED <label expression>`, ISO 19.9 and G111.
    /// It is the label expression a pattern writes after a colon, asked
    /// of an element the query already bound rather than of the rows a
    /// scan is walking.
    IsLabeled {
        expr: Box<Expr>,
        label: LabelExpr,
        negated: bool,
    },
    /// `node IS [NOT] SOURCE OF edge` and its destination twin, ISO
    /// 19.10 and G112.
    IsEndpoint {
        node: Box<Expr>,
        rel: Box<Expr>,
        end: EdgeEnd,
        negated: bool,
    },
    /// `PROPERTY_EXISTS(element, name)`, ISO 19.13 and G115. The name
    /// is written as a name and not as a string, so it is part of the
    /// query rather than a value the query works out.
    PropertyExists {
        expr: Box<Expr>,
        key: String,
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
    /// A graph named where a value goes, ISO 19.6 and GE01.
    ///
    /// It is the same `GraphRef` a `USE` names, in the one other place
    /// the standard lets one stand, and that is the point of the
    /// feature: a graph is a value, so the words that name one are
    /// worth the same wherever they are written. What it answers is a
    /// reference and not the graph, which is what GV60 is.
    GraphRef(GraphRef),
    /// `LET n = a + b IN n * n END`, ISO 20.5 and GE03: a name that
    /// stands for a value for the length of one expression.
    ///
    /// The same [`LetItem`] the clause is made of, because it is the
    /// same thing said in a smaller place: a name for something worked
    /// out once. What the clause adds to a row, this adds to nothing,
    /// and the name is gone at the `END`.
    Let {
        definitions: Vec<LetItem>,
        body: Box<Expr>,
    },
    /// `CAST(expr AS type)`, ISO's GA05.
    Cast {
        expr: Box<Expr>,
        ty: LogicalType,
    },
    /// `CASE`, ISO 20.7 and mandatory feature GE01, in both of the forms
    /// the standard writes it in.
    ///
    /// The searched form asks a condition per branch,
    /// `CASE WHEN n.age < 18 THEN 'child' ELSE 'adult' END`; the simple
    /// form names a value once and compares it with each branch's,
    /// `CASE n.kind WHEN 'a' THEN 1 WHEN 'b' THEN 2 END`, which is the
    /// same expression with the equality written for the reader. A
    /// `CASE` that answers no branch and wrote no `ELSE` is null, which
    /// is what makes the `ELSE` optional.
    Case {
        /// The value the simple form compares each branch with, `None`
        /// for the searched form.
        subject: Option<Box<Expr>>,
        /// The branches, in the order they were written, which is the
        /// order they are asked in.
        branches: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    /// `COALESCE(a, b, c)`, the first of its arguments that is not null.
    /// ISO calls it a case abbreviation, being `CASE WHEN a IS NOT NULL
    /// THEN a ELSE COALESCE(b, c) END` written short.
    Coalesce(Vec<Expr>),
    /// `NULLIF(a, b)`, which is null where the two are equal and `a`
    /// otherwise, the other case abbreviation.
    NullIf {
        value: Box<Expr>,
        compared: Box<Expr>,
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
    /// `EXISTS { MATCH (p:Person) RETURN p.name }`, the third shape the
    /// existence predicate is written in (ISO 19.4): a whole query
    /// rather than a block of matches, asked whether it answered a row.
    ///
    /// The query inside answers a binding table and this asks only
    /// whether that table has a row in it, so what the query returns is
    /// never read. It is a separate variant from [`Expr::Exists`]
    /// because a block of matches folds into the match around it and a
    /// query cannot: it has clauses of its own, ends with a RETURN, and
    /// runs as a query in its own right.
    ExistsQuery(Box<Query>),
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

/// Which end of an edge a predicate asks about (G112).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeEnd {
    Source,
    Destination,
}

impl EdgeEnd {
    /// The word a query writes this end with.
    pub fn text(&self) -> &'static str {
        match self {
            EdgeEnd::Source => "SOURCE",
            EdgeEnd::Destination => "DESTINATION",
        }
    }
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
    /// ISO 20.23, `'ab' || 'cd'`. It binds tighter than a comparison
    /// and looser than an addition, which is where the standard puts
    /// it and what lets `a || b = c` ask about the joined string.
    Concat,
}
