//! Hand-written recursive-descent parser for the zuQL core: MATCH,
//! OPTIONAL MATCH, WHERE, CALL, UNWIND, WITH, RETURN (docs/07 §1,
//! grammar in docs/grammar.ebnf).
//!
//! Keywords match case-insensitively and errors name the position and
//! what was expected. Everything the parser cannot know without the
//! catalog (which variables exist, whether a label is real) is left to
//! the binder. Expression nesting is capped so hostile input cannot
//! overflow the stack: every recursion into a subexpression passes
//! through one depth guard.

use zu_common::gqlstatus::codes;
use zu_common::{Field, LogicalType, RecordType, Result, Temporal, ZuError};

use crate::ast::{
    BinaryOp, CatalogStmt, Clause, Composite, Conjunction, DeleteTarget, EdgeEnd, ElementDefKind,
    ElementTypeDef, Endpoint, Expr, GraphName, GraphRef, GraphTypeRef, GraphTypeSource, Group,
    GroupKind, LabelExpr, LetItem, Linear, Literal, MatchMode, NodePattern, NullOrder, Ordinal,
    PathMode, PathPattern, PatternList, Projection, ProjectionItem, PropertyDef, Query,
    RelDirection, RelPattern, RemoveItem, Removed, Repeat, Selector, SetInto, SetItem, SetOp,
    Simple, SortKey, Statement, Subpath, TxnStmt, UnaryOp, YieldItem,
};
use crate::lexer::{Token, TokenKind, lex};
use crate::value_type;

/// A path being read left to right: the nodes it has reached, the edge
/// patterns between them, and what the brackets around parts of it
/// said.
///
/// It is the parser's working shape rather than the pattern's own,
/// because a path is written as a tree of factors and stored as a line
/// of steps: brackets nest, juxtaposition joins, and both of those are
/// finished with by the time a pattern reaches the binder. A segment
/// always holds one more node than it holds edge patterns, which is
/// what makes `finish` total.
#[derive(Default, Clone)]
struct Segment {
    nodes: Vec<NodePattern>,
    rels: Vec<RelPattern>,
    subpaths: Vec<Subpath>,
    groups: Vec<Group>,
    repeats: Vec<Repeat>,
    filter: Option<Expr>,
}

impl Segment {
    /// The node position the next factor written with no edge in front
    /// of it starts at, which is the one this segment ends at.
    fn here(&self) -> usize {
        self.nodes.len().saturating_sub(1)
    }

    /// Adds a node at the end, joined to whatever is already there: a
    /// node written against the end of a path is the node the path
    /// already ended at, described twice.
    fn join(&mut self, node: NodePattern) {
        match self.nodes.last_mut() {
            None => self.nodes.push(node),
            Some(end) => merge_ends(end, node),
        }
    }

    /// Adds an edge pattern and the factor behind it, which starts a
    /// node of its own rather than joining the one in hand.
    fn step(&mut self, rel: RelPattern, right: Segment) {
        self.rels.push(rel);
        let at = self.nodes.len();
        self.absorb(right, at);
    }

    /// Adds a factor written straight against this one, whose first
    /// node is this one's last (ISO 16.11).
    fn juxtapose(&mut self, right: Segment) {
        let at = self.here();
        let Segment {
            nodes,
            rels,
            subpaths,
            groups,
            repeats,
            filter,
        } = right;
        let mut nodes = nodes.into_iter();
        let head = nodes.next().expect("a factor holds a node");
        self.join(head);
        self.absorb(
            Segment {
                nodes: nodes.collect(),
                rels,
                subpaths,
                groups,
                repeats,
                filter,
            },
            at,
        );
    }

    /// Takes the rest of a factor in, moving the node positions its
    /// brackets pointed at to where they landed here.
    fn absorb(&mut self, right: Segment, at: usize) {
        let step_at = self.rels.len();
        self.nodes.extend(right.nodes);
        self.rels.extend(right.rels);
        for mut sub in right.subpaths {
            sub.from += at;
            sub.to += at;
            self.subpaths.push(sub);
        }
        for mut group in right.groups {
            let shift = match group.kind {
                GroupKind::Node => at,
                GroupKind::Rel => step_at,
            };
            for pos in &mut group.at {
                *pos += shift;
            }
            self.merge_group(group);
        }
        for mut repeat in right.repeats {
            repeat.from += step_at;
            repeat.to += step_at;
            self.repeats.push(repeat);
        }
        self.and(right.filter);
    }

    /// Records a group, joining it to one of the same name already
    /// here. Two stretches that each bound a name go on binding the one
    /// name, so the bindings gather into one list in written order.
    fn merge_group(&mut self, group: Group) {
        match self.groups.iter_mut().find(|seen| seen.name == group.name) {
            Some(seen) if seen.kind == group.kind => seen.at.extend(group.at),
            _ => self.groups.push(group),
        }
    }

    /// Folds one more condition into the ones the brackets wrote.
    fn and(&mut self, filter: Option<Expr>) {
        let Some(next) = filter else { return };
        self.filter = Some(match self.filter.take() {
            None => next,
            Some(seen) => Expr::Binary {
                op: BinaryOp::And,
                lhs: Box::new(seen),
                rhs: Box::new(next),
            },
        });
    }

    /// The pattern this reads as: the first node, then the rest paired
    /// with the edge pattern that reaches them.
    fn finish(self) -> (NodePattern, Vec<(RelPattern, NodePattern)>) {
        let mut nodes = self.nodes.into_iter();
        let start = nodes.next().expect("a path holds a node");
        (start, self.rels.into_iter().zip(nodes).collect())
    }
}

/// Two node patterns that describe one node, made into one.
///
/// It happens where two stretches of a path meet: `((a)-[e]->(b))
/// ((b)-[f]->(c))` walks two edges through three nodes, and the `b`
/// written twice is one node the two brackets each named. What each of
/// them asked of the node stands, so the labels are joined with an `AND`
/// and the properties gather.
///
/// Two different names there are two names for one node, which is legal
/// and is what `(a:Step) ((x:Step)-[:LINK]->(y:Step))` writes: one of
/// them is the pattern's name for the node and the other joins the
/// aliases, and the binder puts both in scope over the one element.
fn merge_ends(end: &mut NodePattern, node: NodePattern) {
    match (&end.var, node.var) {
        (_, None) => {}
        (None, Some(name)) => end.var = Some(name),
        (Some(seen), Some(name)) if *seen == name => {}
        (Some(_), Some(name)) => {
            if !end.aliases.contains(&name) {
                end.aliases.push(name);
            }
        }
    }
    for name in node.aliases {
        if end.var.as_deref() != Some(name.as_str()) && !end.aliases.contains(&name) {
            end.aliases.push(name);
        }
    }
    end.label = match (end.label.take(), node.label) {
        (None, other) | (other, None) => other,
        (Some(seen), Some(next)) => Some(LabelExpr::And(Box::new(seen), Box::new(next))),
    };
    end.props.extend(node.props);
    // Two predicates on the one node are both asked of it, in the order
    // the pattern wrote them.
    end.filter = match (end.filter.take(), node.filter) {
        (None, other) | (other, None) => other,
        (Some(seen), Some(next)) => Some(Box::new(Expr::Binary {
            op: BinaryOp::And,
            lhs: seen,
            rhs: next,
        })),
    };
}

/// The names a repeated stretch binds, and where each repetition binds
/// them (ISO 16.11, feature GQ17).
///
/// A name written inside a stretch that repeats stands for one element
/// per repetition, so it names a list rather than an element. The
/// positions are worked out rather than recorded as the copies are made,
/// because a copy lands a fixed distance behind the one before it: a
/// stretch holding `width` edges advances the node positions by `width`
/// per repetition, and the step positions with them.
fn group_names(
    inner: &Segment,
    count: usize,
    node_at: usize,
    step_at: usize,
    width: usize,
) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    let mut record = |name: &str, kind: GroupKind, local: usize| {
        let base = match kind {
            GroupKind::Node => node_at,
            GroupKind::Rel => step_at,
        };
        let at = (0..count).map(|k| base + k * width + local).collect();
        match groups
            .iter_mut()
            .find(|seen| seen.name == name && seen.kind == kind)
        {
            // A name written twice inside one stretch is one name over
            // both places it was written, so its bindings interleave the
            // way the walk took them.
            Some(seen) => {
                let mut both: Vec<usize> = seen.at.iter().copied().chain(at).collect();
                both.sort_unstable();
                both.dedup();
                seen.at = both;
            }
            None => groups.push(Group {
                name: name.to_string(),
                kind,
                at,
            }),
        }
    };
    for (local, node) in inner.nodes.iter().enumerate() {
        for name in node.var.iter().chain(&node.aliases) {
            record(name, GroupKind::Node, local);
        }
    }
    for (local, rel) in inner.rels.iter().enumerate() {
        if let Some(name) = &rel.var {
            record(name, GroupKind::Rel, local);
        }
    }
    groups
}

/// Drops the names off a copy of a repeated stretch.
///
/// Every repetition binds the same names to elements of its own, so a
/// name cannot stay on the copies: what it stands for is the list of
/// those elements, and that is the group the positions describe. The
/// elements themselves are bound anonymously, which is what they were
/// already for a pattern that named nothing.
fn forget_names(copy: &mut Segment) {
    for node in &mut copy.nodes {
        node.var = None;
        node.aliases.clear();
    }
    for rel in &mut copy.rels {
        rel.var = None;
    }
}

/// What to call a conjunction in a message, spelled the way the query
/// would have written it.
fn conjunction_name(how: Conjunction) -> &'static str {
    match how {
        Conjunction::Otherwise => "OTHERWISE",
        Conjunction::Set { op, .. } => op.keyword(),
    }
}

/// A name written where a value type belongs and spelling none.
fn unknown_type(name: &str) -> ZuError {
    ZuError::gql(codes::C42001, format!("unknown value type '{name}'"))
}

/// Whether a type is one a temporal literal can be written for.
fn is_temporal(ty: &LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::Date
            | LogicalType::LocalTime
            | LogicalType::ZonedTime
            | LogicalType::LocalDatetime
            | LogicalType::ZonedDatetime
            | LogicalType::Duration(_)
    )
}

/// Hard cap on expression nesting; hostile input past it errors instead
/// of overflowing the parser's stack.
///
/// What the number has to be is small enough that the deepest expression
/// the cap admits still fits in the smallest stack a caller is likely to
/// run the parser on, and a level of nesting is one frame per precedence
/// level rather than one frame, so the number moves when the table does.
/// 64 leaves room for the eleven levels the table has now at the frame
/// sizes an unoptimized build gives them, which is the build the cost is
/// worst on. No query a person writes comes near it.
const MAX_DEPTH: usize = 64;

/// How many labels a written key label set may name, impdef IL003.
///
/// A label set is a 64 bit mask, and a written key label set is the
/// labels before the arrow with at least one more after it, so 63 is
/// the most that leaves the whole set a set this file can hold.
const MAX_KEY_LABELS: usize = 63;

/// What an edge type's end refers to. An endpoint written as a bare
/// name and nothing else is a reference to a node type declared
/// elsewhere in the same graph type; anything with a label or a
/// property in it declares the node type where it stands.
fn endpoint(def: ElementTypeDef) -> Endpoint {
    match &def.name {
        Some(name) if def.labels.is_empty() && def.properties.is_empty() => {
            Endpoint::Named(name.clone())
        }
        _ => Endpoint::Inline(Box::new(def)),
    }
}

/// Clause keywords the surface reserves but the v0 core does not parse
/// yet; naming them beats "expected MATCH" when someone writes CREATE.
///
/// The write and transaction statements are here rather than absent
/// because absence is the worse answer: INSERT is how GQL spells the
/// statement that adds an element, so a reader who writes one and is
/// told the parser expected MATCH has been sent looking for a typo
/// instead of a milestone. CREATE is in the list for the opposite
/// reason, being the Cypher spelling of a statement GQL does not have.
const UNIMPLEMENTED: &[&str] = &["CREATE", "MERGE", "SESSION", "FINISH"];

/// How a simple query statement ended, which is what the parser needs
/// to say when something follows that may not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// A result statement, which is `RETURN` today.
    Result,
    /// A write that projected nothing.
    Write,
}

/// Parses one zuQL query.
pub fn parse(source: &str) -> Result<Query> {
    match parse_statement(source)? {
        Statement::Query(q) => Ok(q),
        Statement::Catalog(_) => Err(ZuError::gql(
            codes::C42001,
            "a catalog statement changes the file and answers no rows, so it runs through the session rather than the query path".to_string(),
        )),
        Statement::Transaction(_) => Err(ZuError::gql(
            codes::C42001,
            "a transaction statement says where a transaction begins or ends and reads nothing, so it runs through the session rather than the query path".to_string(),
        )),
    }
}

/// Parses one statement, which is either a query or a catalog
/// statement. The first word tells them apart.
pub fn parse_statement(source: &str) -> Result<Statement> {
    let tokens = lex(source)?;
    let mut parser = Parser {
        source,
        tokens,
        pos: 0,
        depth: 0,
        lists: 0,
    };
    if parser.at_txn_stmt() {
        let stmt = parser.parse_txn_stmt()?;
        return Ok(Statement::Transaction(stmt));
    }
    if parser.at_catalog_stmt() {
        let stmt = parser.parse_catalog_stmt()?;
        return Ok(Statement::Catalog(stmt));
    }
    Ok(Statement::Query(parser.parse_query()?))
}

struct Parser<'a> {
    source: &'a str,
    tokens: Vec<Token>,
    pos: usize,
    depth: usize,
    /// How many pattern lists have been read, which numbers the next
    /// one. A statement block gathers several lists into one clause and
    /// the edges a match mode keeps apart are the ones of a single
    /// list, so each list carries a number of its own.
    lists: u32,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn error(&self, expected: &str) -> ZuError {
        match self.peek() {
            Some(token) => ZuError::gql_in(
                codes::C42001,
                self.source,
                token.start,
                format_args!("expected {expected}, found {}", token.kind.describe()),
            ),
            // The end of the text is a place, but it is not a place any
            // token is at, so this one says so in words and carries no
            // pair rather than pointing one character past the query.
            None => ZuError::gql(
                codes::C42001,
                format!("unexpected end of query, expected {expected}"),
            ),
        }
    }

    /// True when the next token is the given keyword, case-insensitive.
    fn at_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Token { kind: TokenKind::Ident(s), .. }) if s.eq_ignore_ascii_case(kw))
    }

    /// The same test, `offset` tokens further on. Two statements begin
    /// with `CREATE` and the word after it is what tells them apart.
    fn kw_at(&self, offset: usize, kw: &str) -> bool {
        matches!(self.tokens.get(self.pos + offset), Some(Token { kind: TokenKind::Ident(s), .. }) if s.eq_ignore_ascii_case(kw))
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.at_kw(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.error(kw))
        }
    }

    fn at(&self, kind: &TokenKind) -> bool {
        self.peek().is_some_and(|t| t.kind == *kind)
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.at(kind) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind) -> Result<()> {
        if self.eat(kind) {
            Ok(())
        } else {
            Err(self.error(&kind.describe()))
        }
    }

    /// An identifier in name position: unquoted or backticked.
    fn expect_name(&mut self, what: &str) -> Result<String> {
        match self.peek() {
            Some(Token {
                kind: TokenKind::Ident(s),
                ..
            })
            | Some(Token {
                kind: TokenKind::QuotedIdent(s),
                ..
            }) => {
                let name = s.clone();
                self.pos += 1;
                Ok(name)
            }
            _ => Err(self.error(what)),
        }
    }

    /// Whether this statement says where a transaction begins or ends.
    /// `START` opens nothing else, and `COMMIT` and `ROLLBACK` are
    /// whole statements on their own.
    fn at_txn_stmt(&self) -> bool {
        self.at_kw("START") || self.at_kw("COMMIT") || self.at_kw("ROLLBACK")
    }

    /// `START TRANSACTION`, `COMMIT` and `ROLLBACK` (GT01), with the
    /// access mode a start may carry (GT02).
    ///
    /// GQL writes the characteristics as a list because it has more of
    /// them to come; the two that mean something here are the access
    /// modes, and a list naming both modes names no mode, so it is
    /// refused rather than resolved by order.
    fn parse_txn_stmt(&mut self) -> Result<TxnStmt> {
        let stmt = if self.eat_kw("COMMIT") {
            TxnStmt::Commit
        } else if self.eat_kw("ROLLBACK") {
            TxnStmt::Rollback
        } else {
            self.expect_kw("START")?;
            self.expect_kw("TRANSACTION")?;
            let mut read_only = None;
            loop {
                let mode = if self.at_kw("READ") && self.kw_at(1, "ONLY") {
                    true
                } else if self.at_kw("READ") && self.kw_at(1, "WRITE") {
                    false
                } else {
                    break;
                };
                self.pos += 2;
                if read_only.is_some_and(|already| already != mode) {
                    return Err(ZuError::gql(
                        codes::C42001,
                        "a transaction is read only or it is read write, and this one is written both".to_string(),
                    ));
                }
                read_only = Some(mode);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            TxnStmt::Start {
                read_only: read_only.unwrap_or(false),
            }
        };
        self.eat(&TokenKind::Semicolon);
        if let Some(token) = self.peek() {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                token.start,
                format_args!(
                    "nothing may follow a transaction statement, found {}",
                    token.kind.describe()
                ),
            ));
        }
        Ok(stmt)
    }

    /// Whether this statement changes the catalog rather than reading
    /// the graph. `CREATE` opens statements that are not here yet, so
    /// the words after it decide: `GRAPH`, `SCHEMA`, and `PROPERTY
    /// GRAPH` and `OR REPLACE` ahead of either, are all catalog
    /// statements.
    fn at_catalog_stmt(&self) -> bool {
        if !self.at_kw("CREATE") && !self.at_kw("DROP") {
            return false;
        }
        let mut at = 1;
        if self.kw_at(at, "OR") && self.kw_at(at + 1, "REPLACE") {
            at += 2;
        }
        if self.kw_at(at, "PROPERTY") {
            at += 1;
        }
        self.kw_at(at, "GRAPH") || self.kw_at(at, "SCHEMA")
    }

    /// The five statements that change what a file declares: a schema
    /// (GC01), a graph (GC04), and a graph type (GC03), each created or
    /// dropped, with the `IF EXISTS` and `IF NOT EXISTS` modifiers.
    ///
    /// `PROPERTY` is a word the statement may carry and none of them
    /// mean anything different for it, so it is eaten and forgotten:
    /// every graph zu holds is a property graph.
    fn parse_catalog_stmt(&mut self) -> Result<CatalogStmt> {
        let creating = self.eat_kw("CREATE");
        if !creating {
            self.expect_kw("DROP")?;
        }
        let or_replace = if creating && self.at_kw("OR") {
            self.expect_kw("OR")?;
            self.expect_kw("REPLACE")?;
            true
        } else {
            false
        };
        self.eat_kw("PROPERTY");
        let stmt = if self.eat_kw("SCHEMA") {
            self.parse_schema_stmt(creating, or_replace)?
        } else {
            self.expect_kw("GRAPH")?;
            if self.eat_kw("TYPE") {
                self.parse_graph_type_stmt(creating, or_replace)?
            } else {
                self.parse_graph_stmt(creating, or_replace)?
            }
        };
        self.eat(&TokenKind::Semicolon);
        if let Some(token) = self.peek() {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                token.start,
                format_args!(
                    "nothing may follow a catalog statement, found {}",
                    token.kind.describe()
                ),
            ));
        }
        Ok(stmt)
    }

    /// `CREATE GRAPH TYPE` and `DROP GRAPH TYPE` (GC03), from the word
    /// after `TYPE` on.
    fn parse_graph_type_stmt(&mut self, creating: bool, or_replace: bool) -> Result<CatalogStmt> {
        if !creating {
            let if_exists = self.eat_if_exists(false)?;
            let name = self.expect_name("a graph type name")?;
            return Ok(CatalogStmt::DropGraphType { name, if_exists });
        }
        let if_not_exists = self.eat_if_exists(true)?;
        self.check_modifiers(or_replace, if_not_exists)?;
        let name = self.expect_name("a graph type name")?;
        let source = self.parse_graph_type_source()?;
        Ok(CatalogStmt::CreateGraphType {
            name,
            if_not_exists,
            or_replace,
            source,
        })
    }

    /// `CREATE SCHEMA /path` and `DROP SCHEMA /path` (GC01, GC02).
    fn parse_schema_stmt(&mut self, creating: bool, or_replace: bool) -> Result<CatalogStmt> {
        if !creating {
            let if_exists = self.eat_if_exists(false)?;
            let path = self.parse_schema_path()?;
            return Ok(CatalogStmt::DropSchema { path, if_exists });
        }
        let if_not_exists = self.eat_if_exists(true)?;
        self.check_modifiers(or_replace, if_not_exists)?;
        if or_replace {
            return Err(ZuError::gql(
                codes::C42001,
                "a schema is a directory and replacing one would take what it holds with it, so OR REPLACE is not written here".to_string(),
            ));
        }
        let path = self.parse_schema_path()?;
        Ok(CatalogStmt::CreateSchema {
            path,
            if_not_exists,
        })
    }

    /// `CREATE GRAPH` and `DROP GRAPH` (GC04, GC05), from the word
    /// after `GRAPH` on.
    fn parse_graph_stmt(&mut self, creating: bool, or_replace: bool) -> Result<CatalogStmt> {
        if !creating {
            let if_exists = self.eat_if_exists(false)?;
            let name = self.parse_graph_name()?;
            return Ok(CatalogStmt::DropGraph { name, if_exists });
        }
        let if_not_exists = self.eat_if_exists(true)?;
        self.check_modifiers(or_replace, if_not_exists)?;
        let name = self.parse_graph_name()?;
        let of = self.parse_graph_type_ref()?;
        // `AS COPY OF` is what the new graph starts with rather than
        // what it is, so it is read after the type and not instead of
        // it (GG05).
        let copy_of = if self.at_kw("AS") && self.kw_at(1, "COPY") {
            self.expect_kw("AS")?;
            self.expect_kw("COPY")?;
            self.expect_kw("OF")?;
            Some(self.parse_graph_ref()?)
        } else {
            None
        };
        Ok(CatalogStmt::CreateGraph {
            name,
            if_not_exists,
            or_replace,
            of,
            copy_of,
        })
    }

    /// The type a graph is created with. Nothing written is `ANY`,
    /// which is the open graph type every zu1 file has had (GG01).
    fn parse_graph_type_ref(&mut self) -> Result<GraphTypeRef> {
        if self.eat_kw("ANY") {
            // `ANY`, `ANY GRAPH` and `ANY PROPERTY GRAPH` are one
            // spelling of the same open type.
            self.eat_kw("PROPERTY");
            self.eat_kw("GRAPH");
            return Ok(GraphTypeRef::Any);
        }
        if self.at(&TokenKind::Colon)
            && self
                .tokens
                .get(self.pos + 1)
                .is_some_and(|t| t.kind == TokenKind::Colon)
        {
            self.pos += 2;
            return Ok(GraphTypeRef::Named(self.expect_name("a graph type name")?));
        }
        if self.at_kw("TYPED") {
            self.pos += 1;
            return Ok(GraphTypeRef::Named(self.expect_name("a graph type name")?));
        }
        if self.at(&TokenKind::LBrace) || self.at_kw("LIKE") {
            return Ok(GraphTypeRef::Source(self.parse_graph_type_source()?));
        }
        Ok(GraphTypeRef::Any)
    }

    /// A graph's name, which may be written as a path saying which
    /// schema it is in.
    fn parse_graph_name(&mut self) -> Result<GraphName> {
        if !self.at(&TokenKind::Slash) {
            return Ok(GraphName {
                schema: None,
                name: self.expect_name("a graph name")?,
            });
        }
        let mut segments = Vec::new();
        while self.eat(&TokenKind::Slash) {
            segments.push(self.expect_name("a name in a path")?);
        }
        let name = segments.pop().expect("a path has a last segment");
        let schema = if segments.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", segments.join("/"))
        };
        Ok(GraphName {
            schema: Some(schema),
            name,
        })
    }

    /// An absolute directory path, which is what a schema is named by.
    /// The path that is one slash and nothing else is the root schema,
    /// which every file has.
    fn parse_schema_path(&mut self) -> Result<String> {
        if !self.eat(&TokenKind::Slash) {
            return Err(self.error("an absolute directory path"));
        }
        if !self.at_name() {
            return Ok("/".to_string());
        }
        let mut path = String::new();
        loop {
            path.push('/');
            path.push_str(&self.expect_name("a name in a path")?);
            if !self.eat(&TokenKind::Slash) {
                return Ok(path);
            }
        }
    }

    /// Whether a name stands where the parser is, which is what tells
    /// the root path from a path with segments in it.
    fn at_name(&self) -> bool {
        matches!(
            self.peek(),
            Some(Token {
                kind: TokenKind::Ident(_) | TokenKind::QuotedIdent(_),
                ..
            })
        )
    }

    /// `OR REPLACE` takes a taken name over and `IF NOT EXISTS` leaves
    /// it alone, so a statement saying both says nothing.
    fn check_modifiers(&self, or_replace: bool, if_not_exists: bool) -> Result<()> {
        if or_replace && if_not_exists {
            return Err(ZuError::gql(
                codes::C42001,
                "OR REPLACE takes the name over and IF NOT EXISTS leaves it alone, so a statement saying both says nothing".to_string(),
            ));
        }
        Ok(())
    }

    /// `IF NOT EXISTS` when creating, `IF EXISTS` when dropping. The
    /// wrong one of the two is a mistake worth naming, since a `DROP
    /// GRAPH TYPE IF NOT EXISTS` reads like it means something.
    fn eat_if_exists(&mut self, creating: bool) -> Result<bool> {
        if !self.eat_kw("IF") {
            return Ok(false);
        }
        let negated = self.eat_kw("NOT");
        self.expect_kw("EXISTS")?;
        if negated != creating {
            let wanted = if creating {
                "IF NOT EXISTS"
            } else {
                "IF EXISTS"
            };
            return Err(ZuError::gql(
                codes::C42001,
                format!("the modifier here is {wanted}"),
            ));
        }
        Ok(true)
    }

    /// Where the element types come from: written out in braces (GG03),
    /// or read off a graph's tables after `LIKE` (GG04). `AS` before
    /// either is a word ISO allows and neither reading needs.
    fn parse_graph_type_source(&mut self) -> Result<GraphTypeSource> {
        self.eat_kw("AS");
        if self.eat_kw("LIKE") {
            // A zu1 file holds one graph, so the reference here names
            // it whatever it is called and the type is read off the
            // tables rather than off the rows.
            let graph = self.expect_name("the graph a type is taken from")?;
            return Ok(GraphTypeSource::Like(graph));
        }
        self.expect(&TokenKind::LBrace)?;
        let mut elements = Vec::new();
        if !self.at(&TokenKind::RBrace) {
            elements.push(self.parse_element_type()?);
            while self.eat(&TokenKind::Comma) {
                elements.push(self.parse_element_type()?);
            }
        }
        self.expect(&TokenKind::RBrace)?;
        Ok(GraphTypeSource::Elements(elements))
    }

    /// One element type, written the way ISO writes it: as the pattern
    /// an element of it matches.
    ///
    /// `NODE TYPE PersonType (:Person)` is the same type as `(:Person)`
    /// with a name on it (GG20), and an edge type is two node type
    /// patterns with an arc between them.
    fn parse_element_type(&mut self) -> Result<ElementTypeDef> {
        let mut name = None;
        if self.eat_kw("NODE") || self.eat_kw("EDGE") || self.eat_kw("RELATIONSHIP") {
            self.expect_kw("TYPE")?;
            name = Some(self.expect_name("an element type name")?);
        }
        let mut def = self.parse_node_type_pattern()?;
        if !self.at(&TokenKind::Minus) && !self.at(&TokenKind::Lt) && !self.at(&TokenKind::Tilde) {
            if def.name.is_none() {
                def.name = name;
            }
            return Ok(def);
        }
        // What was read as a node type is the left endpoint of an edge
        // type, and the arc says which way the edge points. The name
        // written before the pattern belongs to the edge, not to the
        // endpoint it starts with.
        let from = endpoint(def);
        let (mut edge, undirected, reversed) = self.parse_arc()?;
        let to = endpoint(self.parse_node_type_pattern()?);
        let (from, to) = if reversed { (to, from) } else { (from, to) };
        if edge.name.is_none() {
            edge.name = name;
        }
        edge.kind = ElementDefKind::Edge {
            from,
            to,
            undirected,
        };
        Ok(edge)
    }

    /// `(:Person => :Employee {name :: STRING})`: an alias, the labels
    /// an element of the type is keyed on, the rest of its labels, and
    /// what it declares.
    ///
    /// Everything in it is optional, which is what the GG2x features
    /// are about. The labels before the arrow are the key label set
    /// (GG21); with no arrow written there is no declared key and the
    /// whole label set stands in for one (GG22).
    fn parse_node_type_pattern(&mut self) -> Result<ElementTypeDef> {
        self.expect(&TokenKind::LParen)?;
        let def = self.parse_type_body(true)?;
        self.expect(&TokenKind::RParen)?;
        Ok(def)
    }

    /// The inside of a node type pattern or of an arc's bracket. They
    /// are written the same and differ only in which limit an empty or
    /// oversized key label set raises.
    fn parse_type_body(&mut self, node: bool) -> Result<ElementTypeDef> {
        // The name is what is written before the labels, and everything
        // that could stand there instead says the type has none.
        let anonymous = [
            TokenKind::Colon,
            TokenKind::Eq,
            TokenKind::RParen,
            TokenKind::RBracket,
            TokenKind::LBrace,
        ]
        .iter()
        .any(|kind| self.at(kind));
        let name = if anonymous {
            None
        } else {
            Some(self.expect_name("an element type name")?)
        };
        let mut first = Vec::new();
        if self.eat(&TokenKind::Colon) {
            first = self.parse_label_set()?;
        }
        let mut key_labels = Vec::new();
        let mut labels = first;
        // `=>` is two tokens, the way `->` is: the lexer has no arrow.
        if self.at(&TokenKind::Eq)
            && self
                .tokens
                .get(self.pos + 1)
                .is_some_and(|t| t.kind == TokenKind::Gt)
        {
            self.pos += 2;
            key_labels = std::mem::take(&mut labels);
            self.check_key_labels(&key_labels, node)?;
            self.expect(&TokenKind::Colon)?;
            // The key labels are labels an element carries too, and the
            // catalog keeps one set with the key marked inside it. They
            // go first because that is the order they were written in.
            labels = key_labels.clone();
            for label in self.parse_label_set()? {
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
        }
        let properties = self.parse_property_types()?;
        Ok(ElementTypeDef {
            name,
            kind: ElementDefKind::Node,
            key_labels,
            labels,
            properties,
        })
    }

    /// The limits on a written key label set, which is the one place a
    /// graph type meets a number zu chose (IL003).
    ///
    /// An empty one is written rather than absent, so it is a statement
    /// asking for a type nothing selects, and a set of 64 is one label
    /// short of the label set that has to contain it plus whatever the
    /// arrow adds, which does not fit the 64 bit mask a label set is.
    fn check_key_labels(&self, labels: &[String], node: bool) -> Result<()> {
        if labels.is_empty() {
            let code = if node { codes::C42012 } else { codes::C42014 };
            return Err(ZuError::gql(
                code,
                "a key label set that was written has to name a label".to_string(),
            ));
        }
        if labels.len() > MAX_KEY_LABELS {
            let code = if node { codes::C42013 } else { codes::C42015 };
            return Err(ZuError::gql(
                code,
                format!(
                    "{} key labels, and this file holds {MAX_KEY_LABELS}",
                    labels.len()
                ),
            ));
        }
        Ok(())
    }

    /// `A&B&C` in an element type, which is a label set and not the
    /// label expression a pattern takes: an element type says which
    /// labels an element of it carries, so there is nothing to negate.
    fn parse_label_set(&mut self) -> Result<Vec<String>> {
        let mut labels = vec![self.expect_name("a label")?];
        while self.eat(&TokenKind::Amp) {
            labels.push(self.expect_name("a label")?);
        }
        Ok(labels)
    }

    /// The arc of an edge type pattern, from `-[` or `~[` through the
    /// closing bar and the arrowhead at whichever end has one. Answers
    /// the edge type the bracket describes, whether it is undirected
    /// (GH02), and whether the arrow points back at the endpoint
    /// already read.
    ///
    /// A tilde says the edges of the type have no direction, which is
    /// how a pattern says it too. An arc with no arrowhead says the
    /// same thing, and is the spelling this grammar had before the
    /// tilde arrived.
    fn parse_arc(&mut self) -> Result<(ElementTypeDef, bool, bool)> {
        let reversed = self.eat(&TokenKind::Lt);
        let tilde = self.parse_rel_bar()?;
        self.expect(&TokenKind::LBracket)?;
        let def = self.parse_type_body(false)?;
        self.expect(&TokenKind::RBracket)?;
        if self.parse_rel_bar()? != tilde {
            return Err(self.error("an arc undirected at both ends or at neither"));
        }
        let forward = self.eat(&TokenKind::Gt);
        if reversed && forward {
            return Err(self.error("an arc with one arrowhead"));
        }
        if tilde && (reversed || forward) {
            return Err(self.error("an undirected arc with no arrowhead"));
        }
        Ok((def, tilde || !reversed && !forward, reversed))
    }

    /// `{ name :: TYPE, ... }`, `NO PROPERTIES`, or nothing written at
    /// all, which declares none either way.
    fn parse_property_types(&mut self) -> Result<Vec<PropertyDef>> {
        if self.eat_kw("NO") {
            self.expect_kw("PROPERTIES")?;
            return Ok(Vec::new());
        }
        self.eat_kw("PROPERTIES");
        if !self.eat(&TokenKind::LBrace) {
            return Ok(Vec::new());
        }
        let mut properties = Vec::new();
        if !self.at(&TokenKind::RBrace) {
            loop {
                let name = self.expect_name("a property name")?;
                if !self.eat_kw("TYPED") {
                    self.expect_double_colon()?;
                }
                // A property whose type admits null is one an element
                // may leave out; `NOT NULL` is what says it may not.
                // One rule about null rather than two.
                let ty = self.parse_value_type()?;
                let (ty, optional) = match ty {
                    LogicalType::Nullable(inner) => (*inner, true),
                    other => (other, false),
                };
                properties.push(PropertyDef { name, ty, optional });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RBrace)?;
        Ok(properties)
    }

    /// Whether these clauses change the graph, which is what decides
    /// whether the statement may end without a RETURN.
    /// Whether a statement could end here: the text ran out, or what is
    /// left belongs to somebody else, which is the semicolon after the
    /// statement, the `NEXT` that hands its result to the statement
    /// behind it, and the conjunction that joins it to another operand.
    fn at_statement_end(&self) -> bool {
        self.peek().is_none()
            || self.at(&TokenKind::Semicolon)
            || self.at_kw("NEXT")
            || self.at_conjunction()
    }

    /// Whether a conjunction stands here. Reading one is
    /// [`Parser::parse_conjunction`]; this only looks.
    fn at_conjunction(&self) -> bool {
        self.at_kw("OTHERWISE")
            || self.at_kw("UNION")
            || self.at_kw("EXCEPT")
            || self.at_kw("INTERSECT")
    }

    fn writes(clauses: &[Clause]) -> bool {
        clauses.iter().any(|c| {
            matches!(
                c,
                Clause::Insert { .. }
                    | Clause::Set { .. }
                    | Clause::Remove { .. }
                    | Clause::Delete { .. }
            )
        })
    }

    /// One `REMOVE` item: a property, or the labels an element stops
    /// carrying. `IS` is the other spelling of the colon, the way it is
    /// in a pattern.
    fn parse_remove_item(&mut self) -> Result<RemoveItem> {
        let target = self.expect_name("a variable after REMOVE")?;
        if self.eat(&TokenKind::Colon) || self.eat_kw("IS") {
            let labels = self.parse_label_set()?;
            return Ok(RemoveItem {
                target,
                what: Removed::Labels(labels),
            });
        }
        self.expect(&TokenKind::Dot)?;
        let key = self.expect_name("a property name after the dot")?;
        Ok(RemoveItem {
            target,
            what: Removed::Property(key),
        })
    }

    /// One item of a `SET`: `p.age = 37`, or `p = {age: 37}`, which is
    /// every property of the element rather than one of them.
    ///
    /// The right hand side of the second form is a record written out
    /// rather than any expression that has a record for a value, which is
    /// what the grammar says: the fields are in the statement the way an
    /// `INSERT`'s are. Each field's value is still an expression, so it
    /// can read the element the record is about to replace.
    ///
    /// The third form is `SET p:Admin&Bot`, the labels an element takes
    /// on. There is nothing on the right of it, because what it writes is
    /// written in the statement; the item carries a null there so every
    /// item is still one value per row. `IS` is the other spelling of the
    /// colon, the way it is in a pattern.
    fn parse_set_item(&mut self) -> Result<SetItem> {
        let target = self.expect_name("a variable after SET")?;
        if self.eat(&TokenKind::Colon) || self.eat_kw("IS") {
            let labels = self.parse_label_set()?;
            return Ok(SetItem {
                target,
                into: SetInto::Labels(labels),
                value: Expr::Literal(Literal::Null),
            });
        }
        if self.eat(&TokenKind::Eq) {
            let props = self.parse_property_map()?;
            return Ok(SetItem {
                target,
                into: SetInto::Record,
                value: Expr::Map(props),
            });
        }
        self.expect(&TokenKind::Dot)?;
        let key = self.expect_name("a property name after the dot")?;
        self.expect(&TokenKind::Eq)?;
        let value = self.parse_expr()?;
        Ok(SetItem {
            target,
            into: SetInto::Property(key),
            value,
        })
    }

    /// The counter a `FOR` may number its rows with, if the words for
    /// one are there: `WITH ORDINALITY i` or `WITH OFFSET i`.
    ///
    /// The `WITH` is read two tokens at a time rather than one, because
    /// `WITH` is also a clause and `FOR x IN xs WITH x AS y` is a
    /// projection of the value the `FOR` just bound. Only the word
    /// after it says which of the two was written, so nothing is
    /// consumed until that word has been read.
    fn parse_ordinal(&mut self) -> Result<Option<Ordinal>> {
        let start = if self.at_kw("WITH") && self.kw_at(1, "ORDINALITY") {
            1
        } else if self.at_kw("WITH") && self.kw_at(1, "OFFSET") {
            0
        } else {
            return Ok(None);
        };
        self.pos += 2;
        let name = self.expect_name("a variable name for the counter")?;
        Ok(Some(Ordinal { name, start }))
    }

    /// One definition of a `LET`: a name, an equals sign and the value
    /// the name stands for.
    ///
    /// The name is a plain identifier rather than anything a projection
    /// item may be, because this defines a variable and a variable is a
    /// name. `LET p.age = 30` is a write written where a definition
    /// goes, so it is refused by saying what a definition looks like.
    fn parse_let_item(&mut self) -> Result<LetItem> {
        let name = self.expect_name("a variable name after LET")?;
        if self.at(&TokenKind::Dot) {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                self.peek().expect("peeked").start,
                format_args!(
                    "LET names a value, so the name is a variable of its own; changing a property of an element is SET"
                ),
            ));
        }
        self.expect(&TokenKind::Eq)?;
        let expr = self.parse_expr()?;
        Ok(LetItem { name, expr })
    }

    /// A composite query statement: the `USE` in front of it, and the
    /// linear query statements it joins.
    ///
    /// The conjunctions are left associative and share one level, so
    /// this is a fold rather than a precedence climb: each operand read
    /// joins onto everything read before it.
    fn parse_query(&mut self) -> Result<Query> {
        let (query, ending) = self.parse_query_body()?;
        self.eat(&TokenKind::Semicolon);
        if let Some(token) = self.peek() {
            let what = match ending {
                Ending::Result => "RETURN",
                Ending::Write => "the end of a statement",
            };
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                token.start,
                format_args!("nothing may follow {what}, found {}", token.kind.describe()),
            ));
        }
        Ok(query)
    }

    /// The query a `VALUE { ... }` carries, the brace unconsumed
    /// (GQ18).
    ///
    /// It is a whole query and stops at the closing brace rather than
    /// at the end of the text, and it has to end with a RETURN: a
    /// statement that writes has no result table, and one value is
    /// what this stands for.
    fn parse_nested_query(&mut self) -> Result<Query> {
        self.parse_nested_query_named("VALUE")
    }

    /// The same, for the two words that carry one. An `EXISTS` reads
    /// only whether the query answered a row and a `VALUE` reads the
    /// value in it, but both hold a query that ends with a RETURN, and
    /// the word is carried in so the message names the one that was
    /// written.
    fn parse_nested_query_named(&mut self, word: &str) -> Result<Query> {
        let at = self.peek().map(|t| t.start).unwrap_or(0);
        self.expect(&TokenKind::LBrace)?;
        let (query, ending) = self.parse_query_body()?;
        if ending != Ending::Result {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                at,
                format_args!("a query written inside {word} has to end with RETURN"),
            ));
        }
        self.expect(&TokenKind::RBrace)?;
        Ok(query)
    }

    /// The body of a composite query and how its last statement ended,
    /// stopping wherever the operands run out. What may follow is the
    /// caller's business: the end of the text for a statement, a
    /// closing brace for a nested query.
    fn parse_query_body(&mut self) -> Result<(Query, Ending)> {
        let use_graph = self.parse_use_graph()?;
        let (linear, ending) = self.parse_linear()?;
        let mut body = Composite::Linear(linear);
        let mut ending = ending;
        loop {
            let at = self.peek().map(|t| t.start).unwrap_or(0);
            let Some(how) = self.parse_conjunction()? else {
                break;
            };
            if ending != Ending::Result {
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    at,
                    format_args!(
                        "{} joins two result tables, so the statement in front of it has to end with RETURN",
                        conjunction_name(how)
                    ),
                ));
            }
            let (right, next) = self.parse_linear()?;
            ending = next;
            body = Composite::Conjoined {
                left: Box::new(body),
                how,
                right,
            };
        }
        Ok((Query { use_graph, body }, ending))
    }

    /// A linear query statement: simple statements chained by `NEXT`,
    /// and how the last of them ended.
    fn parse_linear(&mut self) -> Result<(Linear, Ending)> {
        let mut statements = Vec::new();
        loop {
            let (simple, ending) = self.parse_simple()?;
            statements.push(simple);
            if !self.eat_kw("NEXT") {
                return Ok((Linear { statements }, ending));
            }
        }
    }

    /// The operator joining this operand to the one before it, or
    /// `None` where the composite ends.
    ///
    /// A set quantifier that is not written is `DISTINCT`, which is
    /// what the standard says and is why a plain `UNION` removes
    /// duplicates.
    fn parse_conjunction(&mut self) -> Result<Option<Conjunction>> {
        if self.eat_kw("OTHERWISE") {
            return Ok(Some(Conjunction::Otherwise));
        }
        let op = if self.eat_kw("UNION") {
            SetOp::Union
        } else if self.eat_kw("EXCEPT") {
            SetOp::Except
        } else if self.eat_kw("INTERSECT") {
            SetOp::Intersect
        } else {
            return Ok(None);
        };
        let all = if self.eat_kw("ALL") {
            true
        } else {
            self.eat_kw("DISTINCT");
            false
        };
        Ok(Some(Conjunction::Set { op, all }))
    }

    /// One simple query statement: the primitive statements it is
    /// written out of, and the result statement it ends with.
    fn parse_simple(&mut self) -> Result<(Simple, Ending)> {
        let mut clauses = Vec::new();
        loop {
            if self.at_kw("MATCH") || self.at_kw("OPTIONAL") {
                let optional = self.eat_kw("OPTIONAL");
                // GQ21. An OPTIONAL takes either one match statement or
                // a braced block of them. The block is one operand, so
                // it either matches whole or every name it writes is
                // null, which is why the whole block becomes one
                // bracketed group rather than a group per statement.
                let (patterns, filter) = if optional && self.at(&TokenKind::LBrace) {
                    self.parse_match_block(&TokenKind::RBrace)?
                } else {
                    self.expect_kw("MATCH")?;
                    let patterns = self.parse_graph_pattern()?;
                    (patterns, self.parse_where()?)
                };
                clauses.push(Clause::Match {
                    optional,
                    patterns,
                    filter,
                });
                // GQ19. A yield belongs to the match it stands after,
                // and what it does is narrow what the match wrote, so
                // it is a clause of its own here and a statement of the
                // match in the standard. Nothing may stand between the
                // two, which is what makes the two readings the same.
                if self.eat_kw("YIELD") {
                    let mut items = vec![self.parse_yield_item()?];
                    while self.eat(&TokenKind::Comma) {
                        items.push(self.parse_yield_item()?);
                    }
                    clauses.push(Clause::Yield { items });
                }
            } else if self.eat_kw("INSERT") {
                let mut patterns = vec![self.parse_path()?];
                while self.eat(&TokenKind::Comma) {
                    patterns.push(self.parse_path()?);
                }
                clauses.push(Clause::Insert { patterns });
            } else if self.eat_kw("SET") {
                let mut items = vec![self.parse_set_item()?];
                while self.eat(&TokenKind::Comma) {
                    items.push(self.parse_set_item()?);
                }
                clauses.push(Clause::Set { items });
            } else if self.eat_kw("REMOVE") {
                let mut items = vec![self.parse_remove_item()?];
                while self.eat(&TokenKind::Comma) {
                    items.push(self.parse_remove_item()?);
                }
                clauses.push(Clause::Remove { items });
            } else if self.at_kw("DELETE") || self.at_kw("DETACH") || self.at_kw("NODETACH") {
                // NODETACH is the explicit spelling of the default, so
                // it is read and then forgotten: what it says is that
                // the edges do not go, which is what no word says too.
                let detach = self.eat_kw("DETACH");
                if !detach {
                    self.eat_kw("NODETACH");
                }
                self.expect_kw("DELETE")?;
                let mut targets = vec![self.parse_delete_target()?];
                while self.eat(&TokenKind::Comma) {
                    targets.push(self.parse_delete_target()?);
                }
                clauses.push(Clause::Delete { targets, detach });
            } else if self.eat_kw("CALL") {
                let name = self.expect_name("a table function name after CALL")?;
                self.expect(&TokenKind::LParen)?;
                let mut args = Vec::new();
                if !self.at(&TokenKind::RParen) {
                    args.push(self.parse_expr()?);
                    while self.eat(&TokenKind::Comma) {
                        args.push(self.parse_expr()?);
                    }
                }
                self.expect(&TokenKind::RParen)?;
                self.expect_kw("YIELD")?;
                let mut yields = Vec::new();
                loop {
                    let column = self.expect_name("a column name after YIELD")?;
                    let alias = if self.eat_kw("AS") {
                        Some(self.expect_name("an alias after AS")?)
                    } else {
                        None
                    };
                    yields.push((column, alias));
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                clauses.push(Clause::Call { name, args, yields });
            } else if self.eat_kw("UNWIND") {
                // The Cypher spelling, which names the value after the
                // list rather than before it and carries no counter,
                // since WITH ORDINALITY is the standard's word and this
                // form is the one the standard does not have.
                let expr = self.parse_expr()?;
                self.expect_kw("AS")?;
                let alias = self.expect_name("an alias after AS")?;
                clauses.push(Clause::Unwind {
                    expr,
                    alias,
                    ordinal: None,
                });
            } else if self.eat_kw("FOR") {
                let alias = self.expect_name("a variable name after FOR")?;
                self.expect_kw("IN")?;
                let expr = self.parse_expr()?;
                let ordinal = self.parse_ordinal()?;
                clauses.push(Clause::Unwind {
                    expr,
                    alias,
                    ordinal,
                });
            } else if self.eat_kw("FILTER") {
                // The WHERE is the standard's own optional word and
                // says nothing the FILTER has not already said.
                self.eat_kw("WHERE");
                let expr = self.parse_expr()?;
                clauses.push(Clause::Filter { expr });
            } else if self.eat_kw("LET") {
                let mut items = vec![self.parse_let_item()?];
                while self.eat(&TokenKind::Comma) {
                    items.push(self.parse_let_item()?);
                }
                clauses.push(Clause::Let { items });
            } else if self.eat_kw("WITH") {
                let projection = self.parse_projection()?;
                let filter = self.parse_where()?;
                clauses.push(Clause::With { projection, filter });
            } else if self.eat_kw("RETURN") {
                let projection = self.parse_projection()?;
                return Ok((
                    Simple {
                        clauses,
                        result: Some(projection),
                    },
                    Ending::Result,
                ));
            } else if Self::writes(&clauses) && self.at_statement_end() {
                // A write statement is allowed to end without
                // projecting anything, and that is the ordinary way to
                // write one: INSERT (x:Person) is a whole statement and
                // its answer is that it worked. A read query is not
                // allowed to, because a query nobody returns anything
                // from asked a question and threw the answer away.
                return Ok((
                    Simple {
                        clauses,
                        result: None,
                    },
                    Ending::Write,
                ));
            } else if self.at_kw("NEXT") {
                // A read statement in front of NEXT with no RETURN on
                // it has nothing to hand over, and saying that beats
                // listing the clauses that could have come next.
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    self.peek().expect("peeked").start,
                    format_args!(
                        "NEXT reads what the statement in front of it returned, so that statement has to end with RETURN"
                    ),
                ));
            } else if self.at_conjunction() {
                // The same shortfall a word later: a conjunction joins
                // two result tables and the left one is missing.
                let word = ["OTHERWISE", "UNION", "EXCEPT", "INTERSECT"]
                    .into_iter()
                    .find(|kw| self.at_kw(kw))
                    .expect("a conjunction stands here");
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    self.peek().expect("peeked").start,
                    format_args!(
                        "{word} joins two result tables, so the statement in front of it has to end with RETURN"
                    ),
                ));
            } else if let Some(kw) = UNIMPLEMENTED.iter().find(|kw| self.at_kw(kw)) {
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    self.peek().expect("peeked").start,
                    format_args!(
                        "{kw} is not implemented yet, the v0 core is MATCH, WHERE, CALL, UNWIND, WITH, RETURN"
                    ),
                ));
            } else if clauses.is_empty() && self.peek().is_none() {
                return Err(ZuError::gql(codes::C42001, "empty query"));
            } else {
                return Err(self.error("MATCH, OPTIONAL MATCH, CALL, UNWIND, WITH, or RETURN"));
            }
        }
    }

    /// One element a `DELETE` takes away: a variable an earlier clause
    /// bound, or `VALUE { ... }`, the value query expression ISO makes
    /// a delete item out of. Nothing else is one. GQL deletes an
    /// element rather than a value computed from one, so a property
    /// reference here is a syntax error, and it says so rather than the
    /// clause ending at the name and the dot being what nobody
    /// expected.
    fn parse_delete_target(&mut self) -> Result<DeleteTarget> {
        // VALUE is not a reserved word here, so the brace after it is
        // what says this is a value query expression rather than a
        // variable somebody called `value`.
        if self.at_kw("VALUE")
            && self
                .tokens
                .get(self.pos + 1)
                .is_some_and(|token| token.kind == TokenKind::LBrace)
        {
            return self.parse_delete_value();
        }
        Ok(DeleteTarget::Variable(self.parse_delete_variable()?))
    }

    /// `VALUE { <query> }` as a delete item (GD03). The braces are
    /// matched over the tokens rather than by parsing through them, so
    /// the text between them is handed to a fresh parse and comes back
    /// as a query of its own. That is what it is: a nested query
    /// specification runs on its own and answers a value, and running
    /// it is the session's job rather than this clause's.
    fn parse_delete_value(&mut self) -> Result<DeleteTarget> {
        self.expect_kw("VALUE")?;
        let Some(open) = self.peek().filter(|t| t.kind == TokenKind::LBrace).cloned() else {
            return Err(self.error("'{' after VALUE, which opens the query the item deletes"));
        };
        let mut depth = 0usize;
        let mut at = self.pos;
        let close = loop {
            let Some(token) = self.tokens.get(at) else {
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    open.start,
                    format_args!("the '{{' after VALUE is never closed"),
                ));
            };
            match token.kind {
                TokenKind::LBrace => depth += 1,
                TokenKind::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        break token.clone();
                    }
                }
                _ => {}
            }
            at += 1;
        };
        let inner = self.source[open.end..close.start].trim();
        if inner.is_empty() {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                open.start,
                format_args!("VALUE takes a query and the braces after it hold nothing"),
            ));
        }
        // Parsed here rather than at the first run, so that a statement
        // with a broken subquery in it is refused when it is compiled
        // the way every other syntax error is. The error carries the
        // subquery as its text, which is the part it is about.
        let nested = parse(inner)?;
        self.pos = at + 1;
        Ok(DeleteTarget::Value(Box::new(nested)))
    }

    /// The variable form of a delete item.
    fn parse_delete_variable(&mut self) -> Result<String> {
        let name = self.expect_name("a variable after DELETE")?;
        if self.at(&TokenKind::Dot) {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                self.peek().expect("peeked").start,
                format_args!(
                    "DELETE takes away an element and not a property, and '{name}' is followed by one"
                ),
            ));
        }
        Ok(name)
    }

    /// A `USE` clause in front of a query, which says which graph the
    /// clauses after it are against (GQ01, ISO 16.2).
    fn parse_use_graph(&mut self) -> Result<Option<GraphRef>> {
        if !self.eat_kw("USE") {
            return Ok(None);
        }
        Ok(Some(self.parse_graph_ref()?))
    }

    /// The graph a clause names, which is the same thing written in a
    /// `USE` clause and after `AS COPY OF`.
    ///
    /// ISO calls this a graph expression, and the four forms here are
    /// the four that name a graph: the one the session is working in,
    /// the one it started in, one in the catalog by name, and one the
    /// caller passed in. The last is what makes this a reference and
    /// not a word: `USE $g` says which graph only once `$g` is there.
    fn parse_graph_ref(&mut self) -> Result<GraphRef> {
        if self.eat_kw("CURRENT_PROPERTY_GRAPH") || self.eat_kw("CURRENT_GRAPH") {
            return Ok(GraphRef::Current);
        }
        if self.eat_kw("HOME_PROPERTY_GRAPH") || self.eat_kw("HOME_GRAPH") {
            return Ok(GraphRef::Home);
        }
        // `PROPERTY GRAPH` before the name is the long spelling and
        // says nothing the name does not.
        if self.eat_kw("PROPERTY") {
            self.expect_kw("GRAPH")?;
        } else {
            self.eat_kw("GRAPH");
        }
        // A parameter stands where a name stands, and after the
        // optional `GRAPH`, because `USE GRAPH $g` and `USE $g` name
        // the graph the same way.
        if let Some(TokenKind::Param(name)) = self.peek().map(|t| t.kind.clone()) {
            self.pos += 1;
            return Ok(GraphRef::Param(name));
        }
        Ok(GraphRef::Named(self.parse_graph_name()?))
    }

    fn parse_where(&mut self) -> Result<Option<Expr>> {
        if self.eat_kw("WHERE") {
            Ok(Some(self.parse_expr()?))
        } else {
            Ok(None)
        }
    }

    fn parse_projection(&mut self) -> Result<Projection> {
        let distinct = self.eat_kw("DISTINCT");
        let mut star = false;
        let mut items = Vec::new();
        if self.eat(&TokenKind::Star) {
            star = true;
        } else {
            items.push(self.parse_projection_item()?);
        }
        while self.eat(&TokenKind::Comma) {
            items.push(self.parse_projection_item()?);
        }
        // GROUP BY stands after the items and in front of the order,
        // which is where ISO 16.15 puts it: the items say what a row
        // of the group is and this says what a group is.
        let mut group_by = Vec::new();
        if self.eat_kw("GROUP") {
            self.expect_kw("BY")?;
            loop {
                group_by.push(self.parse_expr()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let mut order_by = Vec::new();
        if self.eat_kw("ORDER") {
            self.expect_kw("BY")?;
            loop {
                let expr = self.parse_expr()?;
                let ascending = if self.eat_kw("DESC") || self.eat_kw("DESCENDING") {
                    false
                } else {
                    self.eat_kw("ASC");
                    self.eat_kw("ASCENDING");
                    true
                };
                // GA03. The direction and the null ordering are two
                // independent halves of a sort specification, so DESC
                // NULLS FIRST is a key sorted downwards whose nulls are
                // still at the head.
                let nulls = if self.eat_kw("NULLS") {
                    if self.eat_kw("FIRST") {
                        NullOrder::First
                    } else {
                        self.expect_kw("LAST")?;
                        NullOrder::Last
                    }
                } else {
                    NullOrder::default()
                };
                order_by.push(SortKey {
                    expr,
                    ascending,
                    nulls,
                });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        // OFFSET is the standard's word and SKIP is the synonym ISO
        // 14.9 gives it, so the two are one clause and writing both is
        // writing it twice.
        let skip = if self.eat_kw("OFFSET") || self.eat_kw("SKIP") {
            let expr = self.parse_expr()?;
            if self.at_kw("OFFSET") || self.at_kw("SKIP") {
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    self.peek().expect("peeked").start,
                    format_args!(
                        "OFFSET and SKIP are two spellings of one clause, so a result skips what one of them says and not what both do"
                    ),
                ));
            }
            Some(expr)
        } else {
            None
        };
        let limit = if self.eat_kw("LIMIT") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Projection {
            distinct,
            star,
            items,
            group_by,
            order_by,
            skip,
            limit,
        })
    }

    fn parse_projection_item(&mut self) -> Result<ProjectionItem> {
        let expr = self.parse_expr()?;
        let alias = if self.eat_kw("AS") {
            Some(self.expect_name("an alias after AS")?)
        } else {
            None
        };
        Ok(ProjectionItem { expr, alias })
    }

    // Patterns.

    fn parse_path(&mut self) -> Result<PathPattern> {
        // `p = (a)-...` binds the path; the lookahead keeps a bare
        // pattern unambiguous.
        let var = if matches!(self.peek().map(|t| &t.kind), Some(TokenKind::Ident(_)))
            && matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::Eq)
            ) {
            let name = self.expect_name("a path variable")?;
            self.expect(&TokenKind::Eq)?;
            Some(name)
        } else {
            None
        };
        let (selector, mode) = self.parse_path_prefix()?;
        let mut path = Segment::default();
        self.parse_segment(&mut path)?;
        let subpaths = std::mem::take(&mut path.subpaths);
        let groups = std::mem::take(&mut path.groups);
        let repeats = std::mem::take(&mut path.repeats);
        let filter = path.filter.take();
        let (start, steps) = path.finish();
        Ok(PathPattern {
            var,
            selector,
            mode,
            // The list is said in front of the whole list, so the
            // pattern is read first and stamped after.
            list: PatternList::default(),
            start,
            steps,
            subpaths,
            groups,
            repeats,
            filter,
        })
    }

    /// A stretch of a path: a factor, then whatever follows it, which is
    /// either an edge pattern and another factor or another factor on
    /// its own.
    ///
    /// A factor written straight after another with no edge between them
    /// is ISO's juxtaposition (16.11): the two stretches meet at a node,
    /// and the node the left one ends at is the node the right one
    /// starts at rather than a second node beside it. So the two node
    /// patterns at the join describe one node and are merged into one,
    /// which is what `merge_ends` does.
    fn parse_segment(&mut self, into: &mut Segment) -> Result<()> {
        self.parse_factor(into)?;
        loop {
            if self.at(&TokenKind::Minus) || self.at(&TokenKind::Lt) || self.at(&TokenKind::Tilde) {
                let rel = self.parse_rel()?;
                let mut right = Segment::default();
                self.parse_factor(&mut right)?;
                into.step(rel, right);
                continue;
            }
            // A bracket standing where an edge pattern could have stood
            // is the next factor rather than the next clause, because
            // every clause a pattern can be followed by opens with a
            // word.
            if self.at(&TokenKind::LParen) {
                let mut right = Segment::default();
                self.parse_factor(&mut right)?;
                into.juxtapose(right);
                continue;
            }
            return Ok(());
        }
    }

    /// One factor of a path: a node pattern, or a parenthesized path
    /// pattern (ISO 16.11, feature G038).
    ///
    /// What the brackets may carry is a subpath variable, then a path
    /// mode, then the pattern, then a `WHERE`. A path selector is not on
    /// that list: the standard writes a selector in front of a whole
    /// path pattern, because how many paths to keep is a question about
    /// the answer and not about a stretch of the walk, so one written
    /// here is refused rather than read as though it had been written
    /// outside.
    fn parse_factor(&mut self, into: &mut Segment) -> Result<()> {
        if !self.at_subpath() {
            let node = self.parse_node()?;
            into.join(node);
            return Ok(());
        }
        self.expect(&TokenKind::LParen)?;
        let var = if matches!(self.peek().map(|t| &t.kind), Some(TokenKind::Ident(_)))
            && matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::Eq)
            ) {
            let name = self.expect_name("a subpath variable")?;
            self.expect(&TokenKind::Eq)?;
            Some(name)
        } else {
            None
        };
        let (selector, mode) = self.parse_path_prefix()?;
        if selector.is_some() {
            return Err(ZuError::gql(
                codes::C42001,
                "a path selector says how many of the paths a pattern matches to keep, \
                 which is a question about the whole pattern, so write it in front of \
                 the pattern rather than inside the brackets",
            ));
        }
        let from = into.here();
        let mut inner = Segment::default();
        self.parse_segment(&mut inner)?;
        let filter = self.parse_where()?;
        let close = self.peek().map(|t| t.start).unwrap_or(self.source.len());
        self.expect(&TokenKind::RParen)?;
        let Some(times) = self.parse_edge_quantifier()? else {
            let to = from + inner.rels.len();
            into.juxtapose(inner);
            into.subpaths.push(Subpath {
                var,
                mode,
                from,
                to,
            });
            into.and(filter);
            return Ok(());
        };
        let count = self.fixed_count(times, close)?;
        if var.is_some() {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                close,
                "a repeated stretch is walked once per repetition, so a name written on \
                 it would stand for as many paths as the quantifier asks for rather than \
                 for one, and a list of paths is not a value this engine has",
            ));
        }
        let inline = inner.nodes.iter().any(|node| node.filter.is_some())
            || inner.rels.iter().any(|rel| rel.filter.is_some());
        if filter.is_some() || inner.filter.is_some() || inline {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                close,
                "a condition inside a repeated stretch is asked once per repetition, and \
                 a name inside it stands for that repetition's element rather than for \
                 the group, which is not implemented yet; write the condition behind the \
                 pattern",
            ));
        }
        if inner.subpaths.iter().any(|sub| sub.var.is_some()) {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                close,
                "a name on a stretch inside a repeated one names as many paths as the \
                 quantifier asks for, which is not a value this engine has",
            ));
        }
        let width = inner.rels.len();
        let node_at = from;
        let step_at = into.rels.len();
        for group in group_names(&inner, count, node_at, step_at, width) {
            into.merge_group(group);
        }
        for _ in 0..count {
            let mut copy = inner.clone();
            forget_names(&mut copy);
            into.juxtapose(copy);
        }
        if mode.is_some() {
            into.subpaths.push(Subpath {
                var: None,
                mode,
                from: node_at,
                to: node_at + count * width,
            });
        }
        if width > 0 {
            // The copies are the same step written out several times, so
            // a loop where the stretch begins answers all of them with
            // one edge, and a repeated stretch walks a trail by default.
            into.repeats.push(Repeat {
                from: step_at,
                to: step_at + count * width,
            });
        }
        Ok(())
    }

    /// How many times a quantifier on a stretch repeats it.
    ///
    /// A stretch repeated a fixed number of times is a longer pattern of
    /// the same shape, which is what the parser writes it out as. A
    /// stretch repeated a variable number of times is not: it matches
    /// paths of several lengths, so it stands for as many patterns as
    /// the range holds and the answer is their union. That is the
    /// alternation work and it is not implemented yet, so a range is
    /// refused by name rather than answered for one of its lengths.
    fn fixed_count(&self, times: (Option<u64>, Option<u64>), at: usize) -> Result<usize> {
        let count = match times {
            (Some(min), Some(max)) if min == max => min,
            _ => {
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    at,
                    "a stretch repeated a variable number of times matches paths of \
                     several lengths, which is a union of patterns rather than one \
                     pattern, and that is not implemented yet; write a fixed count",
                ));
            }
        };
        if count == 0 {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                at,
                "a stretch repeated no times at all leaves nothing where the brackets \
                 stood; write the pattern without them",
            ));
        }
        Ok(count as usize)
    }

    /// Whether the `(` in hand opens a parenthesized path pattern rather
    /// than a node pattern.
    ///
    /// The two are told apart by what stands after the bracket, and
    /// three things say a path: another bracket, a name with an `=`
    /// behind it, which is a subpath variable, and a path mode word with
    /// a bracket behind it. Everything else is a node pattern, including
    /// `(WALK)`, which is a node the query called WALK.
    fn at_subpath(&self) -> bool {
        if !self.at(&TokenKind::LParen) {
            return false;
        }
        let next = self.tokens.get(self.pos + 1);
        let after = self.tokens.get(self.pos + 2);
        match next.map(|t| &t.kind) {
            Some(TokenKind::LParen) => true,
            Some(TokenKind::Ident(word)) => match after.map(|t| &t.kind) {
                Some(TokenKind::Eq) => true,
                Some(TokenKind::LParen) => {
                    ["WALK", "TRAIL", "SIMPLE", "ACYCLIC"].contains(&word.to_uppercase().as_str())
                }
                _ => false,
            },
            _ => false,
        }
    }

    /// A graph pattern (ISO 16.9): the match mode in front of a list of
    /// path patterns, and the `KEEP` that may follow it (features G002,
    /// G003, G006 and G007). All three are said once for the whole list
    /// rather than once per pattern of it.
    ///
    /// The match mode says what the list as a whole may bind twice, and
    /// a path mode says what one path of it may, so a list under
    /// `DIFFERENT EDGES` walks trails and no two of its patterns take
    /// the same edge, while one under `REPEATABLE ELEMENTS` walks and
    /// shares whatever it likes. Which list a pattern was written in is
    /// stamped on it here, because a match statement block gathers the
    /// lists of several statements and the edges have to stay apart per
    /// list rather than across the block.
    ///
    /// What a KEEP carries is the prefix a pattern carries itself, and
    /// what it does is fill in what the patterns left out: a pattern
    /// that named no selector takes the KEEP's selector, one that named
    /// no mode takes its mode, and one that named a selector and no mode
    /// keeps the selector and takes the mode. Naming the same kind twice
    /// is refused rather than settled by a rule of precedence, because
    /// either way round would drop something the query asked for.
    fn parse_graph_pattern(&mut self) -> Result<Vec<PathPattern>> {
        let list = PatternList {
            mode: self.parse_match_mode(),
            at: self.lists,
        };
        self.lists += 1;
        let mut patterns = vec![self.parse_path()?];
        while self.eat(&TokenKind::Comma) {
            patterns.push(self.parse_path()?);
        }
        if self.eat_kw("KEEP") {
            let at = self.pos;
            let (selector, mode) = self.parse_path_prefix()?;
            if self.pos == at {
                return Err(ZuError::gql(
                    codes::C42001,
                    "KEEP says which of the paths a pattern matches to keep, \
                     so it needs a path selector or a path mode after it",
                ));
            }
            for path in &mut patterns {
                if selector.is_some() && path.selector.is_some() {
                    return Err(ZuError::gql(
                        codes::C42001,
                        "a pattern carries a path selector and the KEEP names another, \
                         so write one of the two",
                    ));
                }
                if mode.is_some() && path.mode.is_some() {
                    return Err(ZuError::gql(
                        codes::C42001,
                        "a pattern carries a path mode and the KEEP names another, \
                         so write one of the two",
                    ));
                }
                path.selector = path.selector.or(selector);
                path.mode = path.mode.or(mode);
            }
        }
        for path in &mut patterns {
            path.list = list;
        }
        Ok(patterns)
    }

    /// The match mode in front of a pattern list, `DIFFERENT EDGES`
    /// when none is written (ISO 16.9).
    ///
    /// Both are written four ways and all four say the same thing:
    /// `EDGE` and `EDGES` are the same word twice over, and `BINDINGS`
    /// after it is the standard spelling out that what may not repeat is
    /// what the patterns bound rather than what the graph holds. The
    /// singular and the plural of `ELEMENT` go the same way.
    fn parse_match_mode(&mut self) -> MatchMode {
        let at = self.pos;
        if self.eat_kw("REPEATABLE") {
            if self.eat_kw("ELEMENTS") || self.eat_kw("ELEMENT") {
                let _ = self.eat_kw("BINDINGS") || self.eat_kw("BINDING");
                return MatchMode::RepeatableElements;
            }
            // `REPEATABLE` is not a keyword of anything else, but a
            // pattern may name a variable after it, so a word that does
            // not carry on into a match mode is handed back rather than
            // refused here.
            self.pos = at;
        } else if self.eat_kw("DIFFERENT") {
            if self.eat_kw("EDGES") || self.eat_kw("EDGE") {
                let _ = self.eat_kw("BINDINGS") || self.eat_kw("BINDING");
                return MatchMode::DifferentEdges;
            }
            self.pos = at;
        }
        MatchMode::DifferentEdges
    }

    /// What a pattern carries in front of its first node (ISO 16.6): a
    /// path selector, a path mode, or both, and in that order.
    ///
    /// The selector says how many of the paths the pattern matches are
    /// kept per pair of endpoints, and there are seven of them: `ALL`,
    /// `ANY`, `ANY k`, `ALL SHORTEST`, `ANY SHORTEST`, `SHORTEST k` and
    /// `SHORTEST k GROUP`. The mode sits inside the prefix rather than
    /// behind it, so the words come in one fixed order: the selector,
    /// then the mode, then `PATH` or `PATHS`, then `GROUP` last of all.
    ///
    /// `PATH` and `PATHS` are noise the standard allows so that a prefix
    /// reads as English, and they say nothing the rest of it has not
    /// said, so they are eaten and dropped. `GROUP` is no such word: it
    /// is the whole of the difference between keeping k paths and
    /// keeping every path of the k shortest lengths.
    /// A mode of `None` is a pattern that named none, which walks under
    /// the default; the two are told apart because a `KEEP` fills in
    /// what the patterns left out.
    fn parse_path_prefix(&mut self) -> Result<(Option<Selector>, Option<PathMode>)> {
        // The head words, which are all that tells the seven apart bar
        // the `GROUP` at the very end. A bare `SHORTEST` head is held
        // aside as `grouped`, count and all, because whether it counts
        // paths or lengths is not known until that last word is read.
        let mut selector = None;
        let mut grouped = None;
        if self.eat_kw("ALL") {
            // `ALL PATHS` is every path the pattern matches, which is
            // what a pattern with no prefix at all keeps, so it lands
            // where that lands rather than carrying a selector nothing
            // downstream would act on.
            selector = self.eat_kw("SHORTEST").then_some(Selector::AllShortest);
        } else if self.eat_kw("ANY") {
            selector = Some(if self.eat_kw("SHORTEST") {
                Selector::AnyShortest
            } else {
                // `ANY` on its own is `ANY 1`: one path, and the
                // standard leaves which one to the engine.
                Selector::Any(self.take_path_count("ANY")?.unwrap_or(1))
            });
        } else if self.eat_kw("SHORTEST") {
            grouped = Some(self.take_path_count("SHORTEST")?);
        }
        let mode = if self.eat_kw("WALK") {
            Some(PathMode::Walk)
        } else if self.eat_kw("TRAIL") {
            Some(PathMode::Trail)
        } else if self.eat_kw("ACYCLIC") {
            Some(PathMode::Acyclic)
        } else if self.eat_kw("SIMPLE") {
            Some(PathMode::Simple)
        } else {
            None
        };
        self.eat_paths();
        if let Some(count) = grouped {
            let groups = self.eat_kw("GROUPS") || self.eat_kw("GROUP");
            selector = Some(match (count, groups) {
                // A group count left out is one group, and one group is
                // every path of the least length, so bare `SHORTEST
                // GROUPS` is `ALL SHORTEST` written the other way.
                (count, true) => Selector::ShortestGroup(count.unwrap_or(1)),
                (Some(k), false) => Selector::Shortest(k),
                (None, false) => {
                    return Err(ZuError::gql(
                        codes::C42001,
                        "SHORTEST needs a quantity: write SHORTEST k for k paths, \
                         SHORTEST k GROUP for every path of the k least lengths, \
                         or ANY SHORTEST or ALL SHORTEST for one length",
                    ));
                }
            });
        }
        Ok((selector, mode))
    }

    /// The number of paths a selector asks for, refused rather than
    /// obeyed when it is zero: a pattern that keeps no path answers
    /// nothing whatever the graph holds, so it is a query somebody
    /// wrote by mistake.
    ///
    /// The standard gives that its own code, 22G0F invalid number of
    /// paths or groups, rather than the syntax error the rest of a
    /// malformed selector gets: the number is written where a number
    /// belongs and the statement parses, it is the value that is out of
    /// range.
    fn take_path_count(&mut self, word: &str) -> Result<Option<u64>> {
        let at = self.tokens[self.pos.saturating_sub(1)].start;
        match self.take_int() {
            Some(0) => Err(ZuError::gql_in(
                codes::C22G0F,
                self.source,
                at,
                format!("{word} 0 keeps no path at all; a path count starts at 1"),
            )),
            other => Ok(other),
        }
    }

    /// `PATH` or `PATHS` after a selector, which the standard allows and
    /// which means nothing more than the selector already said.
    fn eat_paths(&mut self) {
        let _ = self.eat_kw("PATHS") || self.eat_kw("PATH");
    }

    fn parse_node(&mut self) -> Result<NodePattern> {
        self.expect(&TokenKind::LParen)?;
        let var = match self.peek().map(|t| &t.kind) {
            Some(TokenKind::Ident(_)) | Some(TokenKind::QuotedIdent(_)) => {
                Some(self.expect_name("a variable")?)
            }
            _ => None,
        };
        // A repeated colon is a conjunction, which is how Cypher writes
        // one and what `(n:A:B)` has always meant here.
        let mut label: Option<LabelExpr> = None;
        while self.eat(&TokenKind::Colon) {
            let next = self.parse_label_expr()?;
            label = Some(match label {
                None => next,
                Some(prev) => LabelExpr::And(Box::new(prev), Box::new(next)),
            });
        }
        let props = if self.at(&TokenKind::LBrace) {
            self.parse_property_map()?
        } else {
            Vec::new()
        };
        // G041. A WHERE inside the parentheses is asked of this node,
        // and it may read what the pattern bound to its left, which is
        // what makes it the non local predicate: the node is filtered
        // where it is reached rather than after the whole pattern has
        // been walked.
        let filter = if self.eat_kw("WHERE") {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect(&TokenKind::RParen)?;
        Ok(NodePattern {
            var,
            aliases: Vec::new(),
            label,
            props,
            filter,
        })
    }

    /// A label expression, `|` binding loosest and `!` tightest, the
    /// precedence GQL gives them.
    fn parse_label_expr(&mut self) -> Result<LabelExpr> {
        let mut expr = self.parse_label_and()?;
        while self.eat(&TokenKind::Pipe) {
            let rhs = self.parse_label_and()?;
            expr = LabelExpr::Or(Box::new(expr), Box::new(rhs));
        }
        Ok(expr)
    }

    fn parse_label_and(&mut self) -> Result<LabelExpr> {
        let mut expr = self.parse_label_atom()?;
        while self.eat(&TokenKind::Amp) {
            let rhs = self.parse_label_atom()?;
            expr = LabelExpr::And(Box::new(expr), Box::new(rhs));
        }
        Ok(expr)
    }

    fn parse_label_atom(&mut self) -> Result<LabelExpr> {
        if self.eat(&TokenKind::Bang) {
            return Ok(LabelExpr::Not(Box::new(self.parse_label_atom()?)));
        }
        if self.eat(&TokenKind::Percent) {
            return Ok(LabelExpr::Wildcard);
        }
        if self.eat(&TokenKind::LParen) {
            let inner = self.parse_label_expr()?;
            self.expect(&TokenKind::RParen)?;
            return Ok(inner);
        }
        Ok(LabelExpr::Label(self.expect_name("a label name")?))
    }

    /// Parses the relationship between two nodes. `<-` and `->` are not
    /// lexer tokens, so the arrows are assembled here from `<`, `-`,
    /// and `>`.
    ///
    /// The bar on each side is a dash for an edge that has a direction
    /// and a tilde for one that has none (GH02), which together with
    /// the arrows spell the seven edge patterns of ISO 39075 18.9. The
    /// closing bar may be dropped when no bracket was written, which is
    /// what makes `(a)->(b)` and `(a)~(b)` patterns of their own.
    fn parse_rel(&mut self) -> Result<RelPattern> {
        let inbound = self.eat(&TokenKind::Lt);
        let left_tilde = self.parse_rel_bar()?;
        let bracketed = self.at(&TokenKind::LBracket);
        let (var, types, range, props, filter) = if self.eat(&TokenKind::LBracket) {
            let var = match self.peek().map(|t| &t.kind) {
                Some(TokenKind::Ident(_)) | Some(TokenKind::QuotedIdent(_)) => {
                    Some(self.expect_name("a variable")?)
                }
                _ => None,
            };
            let mut types = Vec::new();
            if self.eat(&TokenKind::Colon) {
                types.push(self.expect_name("a relationship type")?);
                while self.eat(&TokenKind::Pipe) {
                    types.push(self.expect_name("a relationship type")?);
                }
            }
            let range = if self.eat(&TokenKind::Star) {
                Some(self.parse_hop_range()?)
            } else {
                None
            };
            let props = if self.at(&TokenKind::LBrace) {
                self.parse_property_map()?
            } else {
                Vec::new()
            };
            // A WHERE inside the brackets belongs to the step: it is
            // asked of every edge the step walks, one at a time, while
            // the walk is happening. The same text after the pattern
            // would be a different question, asked of whole paths once
            // they have been built.
            let filter = if self.eat_kw("WHERE") {
                Some(Box::new(self.parse_expr()?))
            } else {
                None
            };
            self.expect(&TokenKind::RBracket)?;
            (var, types, range, props, filter)
        } else {
            (None, Vec::new(), None, Vec::new(), None)
        };
        let right_tilde = if bracketed || self.at(&TokenKind::Minus) || self.at(&TokenKind::Tilde) {
            self.parse_rel_bar()?
        } else {
            left_tilde
        };
        if left_tilde != right_tilde {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                self.tokens[self.pos - 1].start,
                "a relationship is undirected at both ends or at neither",
            ));
        }
        let outbound = self.eat(&TokenKind::Gt);
        let arrow = self.tokens[self.pos.saturating_sub(1)].start;
        // The quantifier goes behind the whole arrow, whichever way it
        // points, so it is read here rather than beside the types.
        let range = match (range, self.parse_edge_quantifier()?) {
            (Some(_), Some(_)) => {
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    arrow,
                    "a step repeats as many times as one quantity says: write the hops inside the brackets or the quantifier behind the arrow, not both",
                ));
            }
            (Some(hops), None) => Some(hops),
            (None, quantified) => quantified,
        };
        let direction = match (inbound, left_tilde, outbound) {
            (true, true, true) => {
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    self.tokens[self.pos - 1].start,
                    "an undirected relationship cannot point both ways",
                ));
            }
            (true, false, false) => RelDirection::In,
            (false, false, true) => RelDirection::Out,
            (true, false, true) => RelDirection::AnyDirected,
            (false, true, false) => RelDirection::Undirected,
            (true, true, false) => RelDirection::InOrUndirected,
            (false, true, true) => RelDirection::OutOrUndirected,
            (false, false, false) => RelDirection::Any,
        };
        Ok(RelPattern {
            var,
            types,
            direction,
            range,
            props,
            filter,
        })
    }

    /// One side's bar, `true` for the tilde that says the edges have no
    /// direction of their own.
    fn parse_rel_bar(&mut self) -> Result<bool> {
        if self.eat(&TokenKind::Tilde) {
            return Ok(true);
        }
        self.expect(&TokenKind::Minus)?;
        Ok(false)
    }

    /// The hop range after `*`: nothing, `2`, `1..3`, `..3`, or `2..`.
    fn parse_hop_range(&mut self) -> Result<(Option<u64>, Option<u64>)> {
        let min = self.take_int();
        if self.eat(&TokenKind::DotDot) {
            Ok((min, self.take_int()))
        } else {
            // `*2` is exactly two hops; a bare `*` is unbounded.
            Ok((min, min))
        }
    }

    /// The next token if it is an integer, and nothing otherwise, which
    /// is how both of the ways of writing a repetition read their
    /// bounds: each of theirs is optional.
    fn take_int(&mut self) -> Option<u64> {
        if let Some(Token {
            kind: TokenKind::Int(v),
            ..
        }) = self.peek()
        {
            let v = *v;
            self.pos += 1;
            Some(v)
        } else {
            None
        }
    }

    /// The graph pattern quantifier a step may carry behind its arrow
    /// (ISO 16.10, features G036 and G061): `{n}` for exactly n, `{n,m}`
    /// for a range with either end left out, `+` for one or more, `*`
    /// for zero or more.
    ///
    /// It says what `*n..m` inside the brackets says, so it lands in the
    /// same range and nothing below the parser learns a second way of
    /// writing a repetition. This is the form the standard's own
    /// examples are written in and the form the conformance corpus uses.
    fn parse_edge_quantifier(&mut self) -> Result<Option<(Option<u64>, Option<u64>)>> {
        if self.eat(&TokenKind::Plus) {
            return Ok(Some((Some(1), None)));
        }
        if self.eat(&TokenKind::Star) {
            return Ok(Some((Some(0), None)));
        }
        if !self.at(&TokenKind::LBrace) {
            return Ok(None);
        }
        let brace = self.tokens[self.pos].start;
        self.expect(&TokenKind::LBrace)?;
        let min = self.take_int();
        let range = if self.eat(&TokenKind::Comma) {
            (min, self.take_int())
        } else {
            // `{2}` is exactly two, and a quantifier with no number in
            // it at all says nothing: `{}` is not `*`.
            match min {
                Some(n) => (Some(n), Some(n)),
                None => {
                    return Err(ZuError::gql_in(
                        codes::C42001,
                        self.source,
                        brace,
                        "a quantifier says how many: write {n}, {n,}, {n,m} or a bare + or *",
                    ));
                }
            }
        };
        self.expect(&TokenKind::RBrace)?;
        Ok(Some(range))
    }

    fn parse_property_map(&mut self) -> Result<Vec<(String, Expr)>> {
        self.expect(&TokenKind::LBrace)?;
        let mut props = Vec::new();
        if !self.at(&TokenKind::RBrace) {
            loop {
                let key = self.expect_name("a property name")?;
                self.expect(&TokenKind::Colon)?;
                props.push((key, self.parse_expr()?));
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RBrace)?;
        Ok(props)
    }

    // Expressions, precedence low to high. Every recursion into a
    // subexpression goes through this depth guard.

    fn parse_expr(&mut self) -> Result<Expr> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(ZuError::gql(
                codes::C42001,
                format!("expression nesting deeper than {MAX_DEPTH}"),
            ));
        }
        let result = self.parse_or();
        self.depth -= 1;
        result
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_xor()?;
        while self.eat_kw("OR") {
            let rhs = self.parse_xor()?;
            lhs = binary(BinaryOp::Or, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_xor(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_and()?;
        while self.eat_kw("XOR") {
            let rhs = self.parse_and()?;
            lhs = binary(BinaryOp::Xor, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_not()?;
        while self.eat_kw("AND") {
            let rhs = self.parse_not()?;
            lhs = binary(BinaryOp::And, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_not(&mut self) -> Result<Expr> {
        let mut nots = 0usize;
        while self.eat_kw("NOT") {
            nots += 1;
        }
        let mut expr = self.parse_comparison()?;
        for _ in 0..nots {
            expr = Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(expr),
            };
        }
        Ok(expr)
    }

    fn parse_comparison(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_concat()?;
        loop {
            let op = match self.peek().map(|t| &t.kind) {
                Some(TokenKind::Eq) => BinaryOp::Eq,
                Some(TokenKind::Ne) => BinaryOp::Ne,
                Some(TokenKind::Lt) => BinaryOp::Lt,
                Some(TokenKind::Le) => BinaryOp::Le,
                Some(TokenKind::Gt) => BinaryOp::Gt,
                Some(TokenKind::Ge) => BinaryOp::Ge,
                _ => {
                    if self.eat_kw("IN") {
                        let rhs = self.parse_concat()?;
                        lhs = binary(BinaryOp::In, lhs, rhs);
                        continue;
                    }
                    if self.at_kw("STARTS") {
                        self.pos += 1;
                        self.expect_kw("WITH")?;
                        let rhs = self.parse_concat()?;
                        lhs = binary(BinaryOp::StartsWith, lhs, rhs);
                        continue;
                    }
                    if self.at_kw("ENDS") {
                        self.pos += 1;
                        self.expect_kw("WITH")?;
                        let rhs = self.parse_concat()?;
                        lhs = binary(BinaryOp::EndsWith, lhs, rhs);
                        continue;
                    }
                    if self.eat_kw("CONTAINS") {
                        let rhs = self.parse_concat()?;
                        lhs = binary(BinaryOp::Contains, lhs, rhs);
                        continue;
                    }
                    if self.at_kw("IS") {
                        self.pos += 1;
                        lhs = self.parse_is_tail(lhs)?;
                        continue;
                    }
                    break;
                }
            };
            self.pos += 1;
            let rhs = self.parse_concat()?;
            lhs = binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    /// Everything a query may write behind `IS`, parsed off the line the
    /// comparison is on rather than inside it, since `parse_comparison`
    /// is on the recursion path a nested expression pays for and these
    /// five readings are not.
    fn parse_is_tail(&mut self, lhs: Expr) -> Result<Expr> {
        let negated = self.eat_kw("NOT");
        // GA06. `IS NULL` and `IS TYPED` share their first two words,
        // and NULL is also the name of a type, so the null test is the
        // one written without TYPED and nothing else has to change.
        if self.eat_kw("TYPED") {
            return Ok(Expr::IsTyped {
                expr: Box::new(lhs),
                ty: self.parse_value_type()?,
                negated,
            });
        }
        // G110, G111, G112. The pattern predicates are written after IS
        // as well, and each is settled by the word behind it.
        if self.eat_kw("DIRECTED") {
            return Ok(Expr::IsDirected {
                expr: Box::new(lhs),
                negated,
            });
        }
        if self.eat_kw("LABELED") {
            return Ok(Expr::IsLabeled {
                expr: Box::new(lhs),
                label: self.parse_label_expr()?,
                negated,
            });
        }
        if let Some(end) = self.eat_endpoint() {
            self.expect_kw("OF")?;
            return Ok(Expr::IsEndpoint {
                node: Box::new(lhs),
                rel: Box::new(self.parse_additive()?),
                end,
                negated,
            });
        }
        self.expect_kw("NULL")?;
        Ok(Expr::IsNull {
            expr: Box::new(lhs),
            negated,
        })
    }

    /// ISO 20.23. Concatenation sits between the comparisons and the
    /// additions, which is where the standard puts it: `a || b = c`
    /// asks about the joined string, and `'n=' || 1 + 2` joins the sum
    /// rather than joining the one and adding the two.
    fn parse_concat(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_additive()?;
        while self.eat(&TokenKind::Concat) {
            let rhs = self.parse_additive()?;
            lhs = binary(BinaryOp::Concat, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_additive(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek().map(|t| &t.kind) {
                Some(TokenKind::Plus) => BinaryOp::Add,
                Some(TokenKind::Minus) => BinaryOp::Sub,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_multiplicative()?;
            lhs = binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek().map(|t| &t.kind) {
                Some(TokenKind::Star) => BinaryOp::Mul,
                Some(TokenKind::Slash) => BinaryOp::Div,
                Some(TokenKind::Percent) => BinaryOp::Mod,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_unary()?;
            lhs = binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        // Prefix signs collect iteratively so `----1` cannot recurse.
        let mut negations = 0usize;
        loop {
            if self.eat(&TokenKind::Minus) {
                negations += 1;
            } else if self.eat(&TokenKind::Plus) {
                // Unary plus is a no-op.
            } else {
                break;
            }
        }
        let mut expr = self.parse_postfix()?;
        if negations > 0 {
            // Fold the sign into integer and float literals so `-1` is
            // a literal, not an expression tree.
            expr = match (negations % 2 == 1, expr) {
                (true, Expr::Literal(Literal::Int(v))) => Expr::Literal(Literal::Int(-v)),
                (true, Expr::Literal(Literal::Float(v))) => Expr::Literal(Literal::Float(-v)),
                (false, e @ Expr::Literal(Literal::Int(_)))
                | (false, e @ Expr::Literal(Literal::Float(_))) => e,
                (odd, e) => {
                    let mut wrapped = e;
                    if odd {
                        wrapped = Expr::Unary {
                            op: UnaryOp::Neg,
                            expr: Box::new(wrapped),
                        };
                    }
                    wrapped
                }
            };
        }
        Ok(expr)
    }

    fn parse_postfix(&mut self) -> Result<Expr> {
        let mut expr = self.parse_primary()?;
        while self.eat(&TokenKind::Dot) {
            let key = self.expect_name("a property name after '.'")?;
            expr = Expr::Property {
                base: Box::new(expr),
                key,
            };
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        let token = match self.peek() {
            Some(t) => t.clone(),
            None => return Err(self.error("an expression")),
        };
        match token.kind {
            TokenKind::Int(v) => {
                self.pos += 1;
                // Not a syntax error: the text is a well-formed integer
                // literal, it just names a value no exact numeric type
                // here can hold, which is what 22003 is for.
                let v = i64::try_from(v).map_err(|_| {
                    ZuError::gql_in(
                        codes::C22003,
                        self.source,
                        token.start,
                        "integer literal out of range",
                    )
                })?;
                Ok(Expr::Literal(Literal::Int(v)))
            }
            TokenKind::Float(v) => {
                self.pos += 1;
                Ok(Expr::Literal(Literal::Float(v)))
            }
            TokenKind::Str(s) => {
                self.pos += 1;
                Ok(Expr::Literal(Literal::Str(s)))
            }
            TokenKind::Param(p) => {
                self.pos += 1;
                Ok(Expr::Param(p))
            }
            TokenKind::LParen => {
                self.pos += 1;
                let expr = self.parse_expr()?;
                self.expect(&TokenKind::RParen)?;
                Ok(expr)
            }
            TokenKind::LBracket => {
                self.pos += 1;
                let mut items = Vec::new();
                if !self.at(&TokenKind::RBracket) {
                    loop {
                        items.push(self.parse_expr()?);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                self.expect(&TokenKind::RBracket)?;
                Ok(Expr::List(items))
            }
            TokenKind::LBrace => {
                let props = self.parse_property_map()?;
                Ok(Expr::Map(props))
            }
            TokenKind::Ident(ref name) => {
                let name = name.clone();
                self.pos += 1;
                self.parse_named(name)
            }
            TokenKind::QuotedIdent(ref name) => {
                let name = name.clone();
                self.pos += 1;
                Ok(Expr::Variable(name))
            }
            _ => Err(self.error("an expression")),
        }
    }

    /// An expression that begins with an identifier, the identifier
    /// already read.
    ///
    /// This is its own function rather than an arm of `parse_primary`
    /// because a word can begin four different things and none of them
    /// recurse: a keyword literal, a temporal literal, a call, or a
    /// variable. Keeping them out of the dispatch keeps the frame that
    /// a nested expression pays for on every level down to the
    /// dispatch itself.
    fn parse_named(&mut self, name: String) -> Result<Expr> {
        if name.eq_ignore_ascii_case("null") {
            return Ok(Expr::Literal(Literal::Null));
        }
        if name.eq_ignore_ascii_case("true") {
            return Ok(Expr::Literal(Literal::Bool(true)));
        }
        if name.eq_ignore_ascii_case("false") {
            return Ok(Expr::Literal(Literal::Bool(false)));
        }
        // GE01. CASE is the one word in the expression grammar that
        // opens something with nothing behind it to tell it from a
        // variable read, so it is the case expression wherever an
        // expression begins and is not free to be a name. ISO reserves
        // it, and the alternative is a query that means one thing until
        // somebody names a column `case`.
        if name.eq_ignore_ascii_case("CASE") {
            return self.parse_case();
        }
        // A temporal literal is a type name and a string, and it has to
        // be taken before the name becomes a variable, because DATE is
        // a perfectly good variable name right up until a string
        // follows it.
        if let Some(literal) = self.temporal_literal(&name)? {
            return Ok(literal);
        }
        // GE06, and taken here for the reason the temporal literals
        // are: PATH is a variable name right up until a bracket
        // follows it. Nothing else in the expression grammar puts a
        // bracket after a name, so the two readings never overlap.
        if name.eq_ignore_ascii_case("path") && self.at(&TokenKind::LBracket) {
            self.pos += 1;
            let mut elements = Vec::new();
            if !self.at(&TokenKind::RBracket) {
                loop {
                    elements.push(self.parse_expr()?);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect(&TokenKind::RBracket)?;
            return Ok(Expr::Path(elements));
        }
        // EXISTS carries a match rather than an expression, so it is
        // taken here for the same reason CAST is below: no expression
        // grammar produces a pattern. A brace or a parenthesis has to
        // follow, which leaves `exists` free to be an ordinary variable
        // name everywhere else.
        if name.eq_ignore_ascii_case("EXISTS")
            && (self.at(&TokenKind::LBrace) || self.at(&TokenKind::LParen))
        {
            return self.parse_exists();
        }
        // GQ18, and here for the same reason EXISTS is: VALUE carries
        // a query and no expression grammar produces one. A brace has
        // to follow, so `value` is still an ordinary variable name
        // everywhere else.
        if name.eq_ignore_ascii_case("VALUE") && self.at(&TokenKind::LBrace) {
            return Ok(Expr::ValueQuery(Box::new(self.parse_nested_query()?)));
        }
        if !self.at(&TokenKind::LParen) {
            return Ok(Expr::Variable(name));
        }
        // CAST is written like a call and is not one: its second
        // argument is a type, which no expression grammar can produce,
        // so it is taken here rather than left to a function that would
        // receive a variable named INT8.
        if name.eq_ignore_ascii_case("CAST") {
            self.parse_cast()
        } else if name.eq_ignore_ascii_case("COALESCE") {
            // GE01's two case abbreviations. They are written like
            // calls and are read here rather than left to a function
            // because the binder resolves a function by name and gives
            // every one of them a fixed arity, and because both are
            // short circuiting: COALESCE stops at the first argument
            // that is not null and a function would have had all of
            // them evaluated before it was entered.
            let args = self.parse_arguments()?;
            if args.is_empty() {
                return Err(self.error("at least one argument to COALESCE"));
            }
            Ok(Expr::Coalesce(args))
        } else if name.eq_ignore_ascii_case("NULLIF") {
            let args = self.parse_arguments()?;
            let [value, compared] = <[Expr; 2]>::try_from(args)
                .map_err(|_| self.error("exactly two arguments to NULLIF"))?;
            Ok(Expr::NullIf {
                value: Box::new(value),
                compared: Box::new(compared),
            })
        } else if name.eq_ignore_ascii_case("PROPERTY_EXISTS") {
            // G115, and here for the reason CAST is: the second
            // argument is a property name rather than an expression,
            // and reading it as one would bind a variable that the
            // query never wrote.
            self.parse_property_exists()
        } else {
            self.parse_call(name)
        }
    }

    /// `EXISTS { MATCH (a)-[:knows]->(b) WHERE b.id > 10 }`, the brace
    /// unconsumed and EXISTS already read.
    ///
    /// Everything a full MATCH may say after the patterns is refused
    /// here: an ORDER BY or a LIMIT inside a predicate would be sorting
    /// and cutting a set whose only use is whether it is empty.
    /// One item of a `YIELD`: a variable the match wrote, and the name
    /// it leaves the match under.
    fn parse_yield_item(&mut self) -> Result<YieldItem> {
        let name = self.expect_name("a variable name after YIELD")?;
        let alias = match self.eat_kw("AS") {
            true => Some(self.expect_name("a name after AS")?),
            false => None,
        };
        Ok(YieldItem { name, alias })
    }

    /// The existence predicate in all three of the shapes ISO 19.4
    /// writes it in, the opener unconsumed and EXISTS already read.
    ///
    /// A graph pattern and a match statement block are the same thing
    /// to the parser, a block being a pattern with more matches behind
    /// it, and either may stand in braces or in parentheses. The third
    /// shape is a whole query, which only braces may hold, and it is
    /// told from the other two by the RETURN it has to end with: a
    /// block of matches has no clause that returns anything, so a
    /// RETURN written at the top level of the block is a query and
    /// nothing else it could be.
    fn parse_exists(&mut self) -> Result<Expr> {
        if self.at(&TokenKind::LBrace) && self.block_returns() {
            let query = self.parse_nested_query_named("EXISTS")?;
            return Ok(Expr::ExistsQuery(Box::new(query)));
        }
        let closer = match self.at(&TokenKind::LParen) {
            true => TokenKind::RParen,
            false => TokenKind::RBrace,
        };
        let (patterns, filter) = self.parse_match_block(&closer)?;
        Ok(Expr::Exists {
            patterns,
            filter: filter.map(Box::new),
        })
    }

    /// Whether the block starting at the brace in hand holds a RETURN
    /// of its own, which is what tells a nested query from a block of
    /// matches.
    ///
    /// Only the block's own level counts, so a RETURN inside a query
    /// nested one deeper belongs to that one and is stepped over with
    /// the depth. A name written after a dot is a property and not the
    /// clause however it is spelled, which is the one way the word can
    /// appear here without being the clause.
    fn block_returns(&self) -> bool {
        let mut depth = 0usize;
        let mut after_dot = false;
        for token in &self.tokens[self.pos..] {
            match &token.kind {
                TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket => depth += 1,
                TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return false;
                    }
                }
                TokenKind::Ident(word)
                    if depth == 1 && !after_dot && word.eq_ignore_ascii_case("RETURN") =>
                {
                    return true;
                }
                _ => {}
            }
            after_dot = matches!(token.kind, TokenKind::Dot);
        }
        false
    }

    /// A match statement block, `{ MATCH ... MATCH ... }`, the opener
    /// unconsumed. GQ21 and GQ22 both take one: an `EXISTS` asks
    /// whether it answers a row and an `OPTIONAL` keeps what it
    /// answered, but the block itself is the same thing in both.
    ///
    /// The closer says which brackets hold it, since ISO 19.4 lets an
    /// existence predicate write parentheses where an `OPTIONAL` has
    /// to write braces. They enclose the same block either way.
    ///
    /// The first MATCH is optional because a block that holds one
    /// statement can hold nothing else, so writing it is a courtesy to
    /// the reader rather than something the parser needs.
    ///
    /// The statements are all required and they share the names they
    /// write, so a block is one conjunction: the patterns gather into
    /// one list and the conditions fold together with AND. That fold is
    /// exact because nothing inside a block is optional, so no
    /// statement can hand the next one a null that a condition would
    /// have read differently had it run earlier.
    fn parse_match_block(
        &mut self,
        closer: &TokenKind,
    ) -> Result<(Vec<PathPattern>, Option<Expr>)> {
        let opener = match closer {
            TokenKind::RParen => TokenKind::LParen,
            _ => TokenKind::LBrace,
        };
        self.expect(&opener)?;
        self.eat_kw("MATCH");
        let mut patterns = Vec::new();
        let mut filter: Option<Expr> = None;
        loop {
            patterns.append(&mut self.parse_graph_pattern()?);
            if let Some(next) = self.parse_where()? {
                filter = Some(match filter {
                    Some(seen) => Expr::Binary {
                        op: BinaryOp::And,
                        lhs: Box::new(seen),
                        rhs: Box::new(next),
                    },
                    None => next,
                });
            }
            if !self.eat_kw("MATCH") {
                break;
            }
        }
        self.expect(closer)?;
        Ok((patterns, filter))
    }

    /// `DATE '2024-01-15'` and the rest, the type name already read.
    ///
    /// The type is what says how to read the string, which is why
    /// `TIME '10:00:00'` is refused and `LOCAL TIME '10:00:00'` is not:
    /// a zoned time without an offset is missing the part that makes it
    /// zoned, and guessing UTC would be inventing a fact.
    ///
    /// `None` means this was not a temporal literal at all and the
    /// caller should carry on reading a variable or a call.
    fn temporal_literal(&mut self, name: &str) -> Result<Option<Expr>> {
        let words = match self.peek() {
            Some(Token {
                kind: TokenKind::Ident(second),
                ..
            }) => {
                let joined = format!("{name} {second}");
                let followed = matches!(
                    self.tokens.get(self.pos + 1),
                    Some(Token {
                        kind: TokenKind::Str(_),
                        ..
                    })
                );
                if followed && value_type::is_type_name(&joined) {
                    Some(joined)
                } else {
                    None
                }
            }
            _ => None,
        };
        let (name, skip) = match words {
            Some(joined) => (joined, 1),
            None => (name.to_string(), 0),
        };
        let text = match self.tokens.get(self.pos + skip) {
            Some(Token {
                kind: TokenKind::Str(s),
                ..
            }) => s.clone(),
            _ => return Ok(None),
        };
        let Some(ty) = value_type::spelled(&name, &[]) else {
            return Ok(None);
        };
        if !is_temporal(&ty) {
            return Ok(None);
        }
        self.pos += skip + 1;
        // 22G0H is the duration's own code and 22007 covers the rest of
        // the temporals, so which one this is depends on what was
        // written rather than on where the reading failed.
        let code = match ty {
            LogicalType::Duration(_) => codes::C22G0H,
            _ => codes::C22007,
        };
        let value = Temporal::parse(&ty, &text)
            .ok_or_else(|| ZuError::gql(code, format!("'{text}' is not a {ty} anyone can read")))?;
        Ok(Some(Expr::Literal(Literal::Temporal(value))))
    }

    /// A bracketed, comma separated argument list, the opening
    /// parenthesis unconsumed. The special forms written like calls
    /// share it, since what tells them apart is what they do with the
    /// arguments rather than how the arguments are written.
    fn parse_arguments(&mut self) -> Result<Vec<Expr>> {
        self.expect(&TokenKind::LParen)?;
        let mut args = Vec::new();
        if !self.at(&TokenKind::RParen) {
            loop {
                args.push(self.parse_expr()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RParen)?;
        Ok(args)
    }

    /// `CASE`, ISO 19.4 and GE01, the word already read.
    ///
    /// Both forms are read here, and which one this is is settled by
    /// what follows `CASE`: a `WHEN` means the searched form, and
    /// anything else is the value the simple form compares each branch
    /// with. Nothing is rewritten on the way in, so the simple form
    /// keeps its one subject rather than becoming an equality repeated
    /// once per branch.
    fn parse_case(&mut self) -> Result<Expr> {
        let subject = match self.at_kw("WHEN") {
            true => None,
            false => Some(Box::new(self.parse_expr()?)),
        };
        let mut branches = Vec::new();
        while self.eat_kw("WHEN") {
            let when = self.parse_expr()?;
            self.expect_kw("THEN")?;
            branches.push((when, self.parse_expr()?));
        }
        if branches.is_empty() {
            return Err(self.error("WHEN after CASE"));
        }
        let otherwise = match self.eat_kw("ELSE") {
            true => Some(Box::new(self.parse_expr()?)),
            false => None,
        };
        self.expect_kw("END")?;
        Ok(Expr::Case {
            subject,
            branches,
            otherwise,
        })
    }

    /// `CAST(expr AS type)`, the opening parenthesis unconsumed.
    fn parse_cast(&mut self) -> Result<Expr> {
        self.expect(&TokenKind::LParen)?;
        let expr = self.parse_expr()?;
        self.expect_kw("AS")?;
        let ty = self.parse_value_type()?;
        self.expect(&TokenKind::RParen)?;
        Ok(Expr::Cast {
            expr: Box::new(expr),
            ty,
        })
    }

    /// A value type: one or more components separated by vertical
    /// bars, and an optional `NOT NULL`.
    ///
    /// A type without `NOT NULL` is nullable, which is the standard's
    /// default and not a convenience: `CAST(NULL AS INT)` has to be
    /// null rather than an error, or every optional property that ever
    /// meets a cast becomes one. The wrapper goes on the whole type
    /// rather than on each component, because `INT | STRING NOT NULL`
    /// says one thing about the union and not two about its members.
    fn parse_value_type(&mut self) -> Result<LogicalType> {
        let first = self.parse_type_component()?;
        // GV67, the closed dynamic union. One component is a type and
        // not a union of one, so a query that never writes a bar never
        // pays for the vector.
        let ty = if self.at(&TokenKind::Pipe) {
            let mut members = vec![first];
            while self.eat(&TokenKind::Pipe) {
                members.push(self.parse_type_component()?);
            }
            LogicalType::Union(members)
        } else {
            first
        };
        if self.eat_kw("NOT") {
            self.expect_kw("NULL")?;
            return Ok(ty);
        }
        Ok(LogicalType::Nullable(Box::new(ty)))
    }

    /// One member of a value type.
    ///
    /// Most of these are a name and its arguments, which is a table
    /// lookup. The ones that are not are the types with structure in
    /// them: `ANY` is a prefix rather than a name, and a record type
    /// carries a list of fields that are themselves types.
    fn parse_type_component(&mut self) -> Result<LogicalType> {
        // GV47, GV60, GV66 and GV68 all begin with ANY, and it opens
        // whatever follows it rather than naming a type of its own,
        // except when nothing follows, where it is the open union.
        if self.eat_kw("ANY") {
            if self.eat_kw("RECORD") {
                return Ok(LogicalType::Record(RecordType::open(Vec::new())));
            }
            if self.eat_kw("GRAPH") {
                return Ok(LogicalType::Graph(None));
            }
            // GV61's open spelling, with `BINDING` optional the same
            // way it is on the bare name.
            if self.eat_kw("BINDING") {
                self.expect_kw("TABLE")?;
                return Ok(LogicalType::BindingTable(None));
            }
            if self.eat_kw("TABLE") {
                return Ok(LogicalType::BindingTable(None));
            }
            if self.eat_kw("PROPERTY") {
                // `ANY PROPERTY GRAPH` is GV60 written the long way and
                // `ANY PROPERTY VALUE` is GV68, which is why the word
                // after PROPERTY is read rather than assumed.
                if self.eat_kw("GRAPH") {
                    return Ok(LogicalType::Graph(None));
                }
                self.expect_kw("VALUE")?;
                return Ok(LogicalType::AnyProperty);
            }
            self.eat_kw("VALUE");
            return Ok(LogicalType::Any);
        }
        if self.eat_kw("RECORD") {
            return Ok(LogicalType::Record(self.parse_record_type()?));
        }
        // GV50, written in front of its element type.
        if self.at_list_name() {
            return self.parse_list_type(None);
        }
        let mut name = match self.peek() {
            Some(Token {
                kind: TokenKind::Ident(s),
                ..
            }) => s.clone(),
            _ => return Err(self.error("a value type")),
        };
        self.pos += 1;
        // A few names are two words. The pair is taken only when the
        // pair is itself a name, so DOUBLE PRECISION is one type and a
        // bare DOUBLE followed by anything else is another.
        if let Some(Token {
            kind: TokenKind::Ident(second),
            ..
        }) = self.peek()
        {
            let joined = format!("{name} {second}");
            if value_type::is_type_name(&joined) {
                self.pos += 1;
                name = joined;
            }
        }
        if !value_type::is_type_name(&name) {
            return Err(unknown_type(&name));
        }
        let mut args = Vec::new();
        if self.eat(&TokenKind::LParen) {
            loop {
                args.push(self.parse_type_argument()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RParen)?;
        }
        let ty = value_type::spelled(&name, &args).ok_or_else(|| {
            let written: Vec<String> = args.iter().map(u32::to_string).collect();
            ZuError::gql(
                codes::C42001,
                format!(
                    "'{name}' does not take ({}), or the numbers in it are out of range",
                    written.join(", ")
                ),
            )
        })?;
        // GV50 again, written after its element type. `INT LIST` and
        // `LIST<INT>` are one type asked for two ways, so the postfix
        // spelling is read here rather than given a branch of its own.
        if self.at_list_name() {
            return self.parse_list_type(Some(ty));
        }
        Ok(ty)
    }

    /// Whether a list type name starts here.
    ///
    /// `LIST` and `ARRAY` are the same type spelled twice, and `GROUP`
    /// in front of either is the spelling an aggregation uses, which
    /// names the same type as well.
    fn at_list_name(&self) -> bool {
        let named = |offset: usize| {
            matches!(
                self.tokens.get(self.pos + offset),
                Some(Token {
                    kind: TokenKind::Ident(s),
                    ..
                }) if s.eq_ignore_ascii_case("LIST") || s.eq_ignore_ascii_case("ARRAY")
            )
        };
        named(0) || (self.at_kw("GROUP") && named(1))
    }

    /// A list type, GV50, from either of its two spellings.
    ///
    /// `elem` is the element type when it was written in front, as in
    /// `INT LIST`, and `None` when the name came first and the element
    /// type is either inside the angle brackets or not written at all.
    /// A list type with no element type admits a list of anything, and
    /// an element type is nullable unless it says otherwise, which is
    /// why `[1, null] IS TYPED LIST<INT>` is true and the question a
    /// query usually means to ask is `LIST<INT NOT NULL>`.
    fn parse_list_type(&mut self, elem: Option<LogicalType>) -> Result<LogicalType> {
        self.eat_kw("GROUP");
        self.pos += 1;
        let elem = match elem {
            Some(ty) => LogicalType::Nullable(Box::new(ty)),
            None if self.eat(&TokenKind::Lt) => {
                let ty = self.parse_value_type()?;
                self.expect(&TokenKind::Gt)?;
                ty
            }
            None => LogicalType::Any,
        };
        // The one constraint a list type carries is a maximum length,
        // and it is a count rather than an expression for the same
        // reason a string's length is.
        let max = if self.eat(&TokenKind::LParen) {
            let n = self.parse_type_argument()?;
            self.expect(&TokenKind::RParen)?;
            Some(n)
        } else {
            None
        };
        Ok(LogicalType::List {
            elem: Box::new(elem),
            max,
        })
    }

    /// The fields of a record type, GV46, or no fields at all.
    ///
    /// A record type written without a field list is the open record
    /// type of GV47, which admits records with any fields, so a bare
    /// `RECORD` and `ANY RECORD` are the same type. GV48, a record
    /// inside a record, needs nothing of its own: a field's type is a
    /// value type and a record type is one.
    fn parse_record_type(&mut self) -> Result<RecordType> {
        if !self.eat(&TokenKind::LBrace) {
            return Ok(RecordType::open(Vec::new()));
        }
        let mut fields = Vec::new();
        if !self.at(&TokenKind::RBrace) {
            loop {
                let name = self.expect_name("a field name")?;
                self.expect_double_colon()?;
                let ty = self.parse_value_type()?;
                fields.push(Field { name, ty });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RBrace)?;
        Ok(RecordType::closed(fields))
    }

    /// The `::` that separates a field name from its type.
    ///
    /// The lexer has no token for it, so this is two colons, and they
    /// have to be adjacent: `a : : INT` is not a field and reading it
    /// as one would let a typo through as a type.
    fn expect_double_colon(&mut self) -> Result<()> {
        let adjacent = match (self.tokens.get(self.pos), self.tokens.get(self.pos + 1)) {
            (Some(first), Some(second)) => {
                first.kind == TokenKind::Colon
                    && second.kind == TokenKind::Colon
                    && first.start + 1 == second.start
            }
            _ => false,
        };
        if !adjacent {
            return Err(self.error("'::'"));
        }
        self.pos += 2;
        Ok(())
    }

    /// One number inside a type's parentheses: a digit count, a scale
    /// or a length, all of which are counts and none of which is an
    /// expression.
    fn parse_type_argument(&mut self) -> Result<u32> {
        let value = match self.peek() {
            Some(Token {
                kind: TokenKind::Int(v),
                ..
            }) => *v,
            _ => return Err(self.error("a count")),
        };
        self.pos += 1;
        u32::try_from(value).map_err(|_| self.error("a count that fits in 32 bits"))
    }

    /// The end of an edge a source or destination predicate names,
    /// consumed when one of the two words stands here (G112).
    fn eat_endpoint(&mut self) -> Option<EdgeEnd> {
        if self.eat_kw("SOURCE") {
            return Some(EdgeEnd::Source);
        }
        self.eat_kw("DESTINATION").then_some(EdgeEnd::Destination)
    }

    /// `PROPERTY_EXISTS(element, name)`, the parenthesis unconsumed and
    /// the name already read. A quoted name is taken as well, because a
    /// property whose name needs quoting is still a property.
    fn parse_property_exists(&mut self) -> Result<Expr> {
        self.expect(&TokenKind::LParen)?;
        let expr = Box::new(self.parse_expr()?);
        self.expect(&TokenKind::Comma)?;
        let key = self.expect_name("a property name")?;
        self.expect(&TokenKind::RParen)?;
        Ok(Expr::PropertyExists { expr, key })
    }

    fn parse_call(&mut self, name: String) -> Result<Expr> {
        self.expect(&TokenKind::LParen)?;
        let distinct = self.eat_kw("DISTINCT");
        if self.eat(&TokenKind::Star) {
            self.expect(&TokenKind::RParen)?;
            return Ok(Expr::Call {
                name,
                distinct,
                star: true,
                args: Vec::new(),
            });
        }
        let mut args = Vec::new();
        if !self.at(&TokenKind::RParen) {
            loop {
                args.push(self.parse_expr()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RParen)?;
        Ok(Expr::Call {
            name,
            distinct,
            star: false,
            args,
        })
    }
}

fn binary(op: BinaryOp, lhs: Expr, rhs: Expr) -> Expr {
    Expr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zu_common::Position;

    fn parsed(source: &str) -> Query {
        parse(source).expect("parse")
    }

    /// The one linear statement of a query that has no conjunction in
    /// it, for the tests that are about what is inside an operand
    /// rather than about how operands are joined.
    fn linear_body(query: &Query) -> &Linear {
        match &query.body {
            Composite::Linear(linear) => linear,
            Composite::Conjoined { .. } => panic!("this query is composite"),
        }
    }

    fn parse_err(source: &str) -> String {
        parse(source).expect_err("should fail").to_string()
    }

    fn catalog_stmt(source: &str) -> CatalogStmt {
        match parse_statement(source).expect("parse") {
            Statement::Catalog(stmt) => stmt,
            other => panic!("parsed as {other:?}"),
        }
    }

    fn txn_stmt(source: &str) -> TxnStmt {
        match parse_statement(source).expect("parse") {
            Statement::Transaction(stmt) => stmt,
            other => panic!("parsed as {other:?}"),
        }
    }

    fn catalog_err(source: &str) -> String {
        parse_statement(source)
            .expect_err("should fail")
            .to_string()
    }

    /// The labels an endpoint written out in the pattern carries.
    fn labels_of(end: &Endpoint) -> Vec<String> {
        match end {
            Endpoint::Inline(def) => def.labels.clone(),
            Endpoint::Named(name) => panic!("'{name}' is a reference, not a pattern"),
        }
    }

    /// The type an `IS TYPED` in the first return item is checking,
    /// with the nullability a type written without NOT NULL picks up
    /// dropped, since what is under test here is the spelling.
    fn typed_against(source: &str) -> LogicalType {
        let q = parsed(source);
        let projection = q.result().expect("RETURN");
        let ty = match &projection.items[0].expr {
            Expr::IsTyped { ty, .. } => ty.clone(),
            other => panic!("parsed as {other:?}"),
        };
        match ty {
            LogicalType::Nullable(inner) => *inner,
            other => other,
        }
    }

    /// GV60 and GV61 are each written four ways, two of them opened by
    /// ANY, and all four name the one type. ANY reads the word after
    /// PROPERTY rather than assuming it, because `ANY PROPERTY GRAPH`
    /// and `ANY PROPERTY VALUE` part company there.
    #[test]
    fn a_reference_type_is_read_in_all_of_its_spellings() {
        let graph = LogicalType::Graph(None);
        let table = LogicalType::BindingTable(None);
        assert_eq!(typed_against("RETURN 1 IS TYPED GRAPH"), graph);
        assert_eq!(typed_against("RETURN 1 IS TYPED PROPERTY GRAPH"), graph);
        assert_eq!(typed_against("RETURN 1 IS TYPED ANY GRAPH"), graph);
        assert_eq!(typed_against("RETURN 1 IS TYPED ANY PROPERTY GRAPH"), graph);
        assert_eq!(typed_against("RETURN 1 IS TYPED BINDING TABLE"), table);
        assert_eq!(typed_against("RETURN 1 IS TYPED TABLE"), table);
        assert_eq!(typed_against("RETURN 1 IS TYPED ANY BINDING TABLE"), table);
        assert_eq!(typed_against("RETURN 1 IS TYPED ANY TABLE"), table);
        assert_eq!(
            typed_against("RETURN 1 IS TYPED ANY PROPERTY VALUE"),
            LogicalType::AnyProperty
        );
    }

    fn text_type() -> LogicalType {
        LogicalType::Str {
            min: None,
            max: None,
            fixed: false,
        }
    }

    #[test]
    fn a_graph_type_is_created_from_the_element_type_patterns_it_holds() {
        let stmt = catalog_stmt(
            "CREATE PROPERTY GRAPH TYPE IF NOT EXISTS social {
               NODE TYPE PersonType (:Person => :Employee
                 {name :: STRING NOT NULL, nickname :: STRING}),
               (:Org),
               (:Person)-[:KNOWS => :Close]->(:Org),
               (:Person)<-[:EMPLOYS]-(:Org),
               (:Person)-[:MEETS]-(:Person),
               (:Person)~[:SITS_WITH]~(:Person)
             }",
        );
        let CatalogStmt::CreateGraphType {
            name,
            if_not_exists,
            or_replace,
            source,
        } = stmt
        else {
            panic!("not a create");
        };
        assert_eq!(name, "social");
        assert!(if_not_exists);
        assert!(!or_replace);
        let GraphTypeSource::Elements(elements) = source else {
            panic!("not written out");
        };
        assert_eq!(elements.len(), 6);
        assert_eq!(elements[0].name.as_deref(), Some("PersonType"));
        assert_eq!(elements[0].key_labels, ["Person"]);
        // The key labels are labels the element carries as well, so the
        // label set holds both and the key is the part before the arrow.
        assert_eq!(elements[0].labels, ["Person", "Employee"]);
        // A type that admits null is one an element may leave out.
        assert_eq!(
            elements[0].properties,
            vec![
                PropertyDef {
                    name: "name".into(),
                    ty: text_type(),
                    optional: false,
                },
                PropertyDef {
                    name: "nickname".into(),
                    ty: text_type(),
                    optional: true,
                },
            ]
        );
        // No arrow, so nothing is declared and the whole label set
        // stands in for the key.
        assert!(elements[1].name.is_none());
        assert!(elements[1].key_labels.is_empty());
        assert_eq!(elements[1].labels, ["Org"]);
        let ElementDefKind::Edge {
            from,
            to,
            undirected,
        } = &elements[2].kind
        else {
            panic!("not an edge");
        };
        assert!(!undirected);
        assert_eq!(labels_of(from), ["Person"]);
        assert_eq!(labels_of(to), ["Org"]);
        assert_eq!(elements[2].key_labels, ["KNOWS"]);
        // An arrow pointing left is the same edge type read the other
        // way round, so the endpoints come out in the order it means.
        let ElementDefKind::Edge { from, to, .. } = &elements[3].kind else {
            panic!("not an edge");
        };
        assert_eq!(labels_of(from), ["Org"]);
        assert_eq!(labels_of(to), ["Person"]);
        let ElementDefKind::Edge { undirected, .. } = &elements[4].kind else {
            panic!("not an edge");
        };
        assert!(undirected, "an arc with no arrowhead is undirected");
        // And the tilde says the same thing, which is how a pattern
        // says it (GH02).
        let ElementDefKind::Edge { undirected, .. } = &elements[5].kind else {
            panic!("not an edge");
        };
        assert!(undirected, "a tilde arc is undirected");
        assert_eq!(elements[5].labels, ["SITS_WITH"]);
        // The two spellings do not mix, and neither takes an arrowhead.
        assert!(
            parse_err("CREATE GRAPH TYPE t { (:A)~[:R]-(:A) }")
                .contains("an arc undirected at both ends or at neither")
        );
        assert!(
            parse_err("CREATE GRAPH TYPE t { (:A)~[:R]~>(:A) }")
                .contains("an undirected arc with no arrowhead")
        );
    }

    #[test]
    fn a_graph_type_may_be_taken_from_a_graph_or_dropped() {
        assert_eq!(
            catalog_stmt("CREATE GRAPH TYPE mirror LIKE social"),
            CatalogStmt::CreateGraphType {
                name: "mirror".into(),
                if_not_exists: false,
                or_replace: false,
                source: GraphTypeSource::Like("social".into()),
            }
        );
        assert_eq!(
            catalog_stmt("CREATE OR REPLACE PROPERTY GRAPH TYPE mirror AS LIKE social"),
            CatalogStmt::CreateGraphType {
                name: "mirror".into(),
                if_not_exists: false,
                or_replace: true,
                source: GraphTypeSource::Like("social".into()),
            }
        );
        assert_eq!(
            catalog_stmt("DROP PROPERTY GRAPH TYPE IF EXISTS mirror;"),
            CatalogStmt::DropGraphType {
                name: "mirror".into(),
                if_exists: true,
            }
        );
        // An endpoint written as a name alone points at a type declared
        // elsewhere rather than declaring one.
        let stmt = catalog_stmt(
            "CREATE GRAPH TYPE t { EDGE TYPE Knows (PersonType)-[:KNOWS]->(PersonType) }",
        );
        let CatalogStmt::CreateGraphType { source, .. } = stmt else {
            panic!("not a create");
        };
        let GraphTypeSource::Elements(elements) = source else {
            panic!("not written out");
        };
        assert_eq!(elements[0].name.as_deref(), Some("Knows"));
        let ElementDefKind::Edge { from, .. } = &elements[0].kind else {
            panic!("not an edge");
        };
        assert_eq!(*from, Endpoint::Named("PersonType".into()));
    }

    #[test]
    fn a_key_label_set_that_was_written_has_a_size_this_file_holds() {
        // Written and empty is a type nothing selects, and the node and
        // the edge case have a condition each.
        assert!(catalog_err("CREATE GRAPH TYPE t { (=> :Narrow) }").starts_with("42012"));
        assert!(
            catalog_err("CREATE GRAPH TYPE t { (:A)-[=> :Narrow]->(:B) }").starts_with("42014")
        );
        let wide = (1..=64)
            .map(|n| format!("K{n:02}"))
            .collect::<Vec<_>>()
            .join("&");
        assert!(
            catalog_err(&format!("CREATE GRAPH TYPE t {{ (:{wide} => :Wide) }}"))
                .starts_with("42013")
        );
        assert!(
            catalog_err(&format!(
                "CREATE GRAPH TYPE t {{ (:N)-[:{wide} => :Wide]->(:N) }}"
            ))
            .starts_with("42015")
        );
    }

    #[test]
    fn a_schema_and_a_graph_are_catalog_objects_of_their_own() {
        assert_eq!(
            catalog_stmt("CREATE SCHEMA /app"),
            CatalogStmt::CreateSchema {
                path: "/app".into(),
                if_not_exists: false,
            }
        );
        assert_eq!(
            catalog_stmt("CREATE SCHEMA IF NOT EXISTS /app/inner"),
            CatalogStmt::CreateSchema {
                path: "/app/inner".into(),
                if_not_exists: true,
            }
        );
        assert_eq!(
            catalog_stmt("DROP SCHEMA IF EXISTS /"),
            CatalogStmt::DropSchema {
                path: "/".into(),
                if_exists: true,
            }
        );
        // GG01: the open graph type, whichever way it is spelled.
        for source in [
            "CREATE GRAPH g ANY",
            "CREATE PROPERTY GRAPH g ANY GRAPH",
            "CREATE GRAPH g ANY PROPERTY GRAPH",
            "CREATE GRAPH g",
        ] {
            assert_eq!(
                catalog_stmt(source),
                CatalogStmt::CreateGraph {
                    name: GraphName {
                        schema: None,
                        name: "g".into(),
                    },
                    if_not_exists: false,
                    or_replace: false,
                    of: GraphTypeRef::Any,
                    copy_of: None,
                },
                "{source}"
            );
        }
        assert_eq!(
            catalog_stmt("CREATE GRAPH IF NOT EXISTS /app/social :: social_type"),
            CatalogStmt::CreateGraph {
                name: GraphName {
                    schema: Some("/app".into()),
                    name: "social".into(),
                },
                if_not_exists: true,
                or_replace: false,
                of: GraphTypeRef::Named("social_type".into()),
                copy_of: None,
            }
        );
        // GG05: what the graph starts with, read after what it is.
        assert_eq!(
            catalog_stmt("CREATE OR REPLACE GRAPH g ANY AS COPY OF h"),
            CatalogStmt::CreateGraph {
                name: GraphName {
                    schema: None,
                    name: "g".into(),
                },
                if_not_exists: false,
                or_replace: true,
                of: GraphTypeRef::Any,
                copy_of: Some(GraphRef::Named(GraphName {
                    schema: None,
                    name: "h".into(),
                })),
            }
        );
        // The graph the statement is against is a graph to copy like
        // any other, and the one a copy of a loaded file means.
        assert_eq!(
            catalog_stmt("CREATE GRAPH g ANY AS COPY OF CURRENT_PROPERTY_GRAPH"),
            CatalogStmt::CreateGraph {
                name: GraphName {
                    schema: None,
                    name: "g".into(),
                },
                if_not_exists: false,
                or_replace: false,
                of: GraphTypeRef::Any,
                copy_of: Some(GraphRef::Current),
            }
        );
        // GG03 and GG04: a type written where the graph is created.
        let CatalogStmt::CreateGraph { of, .. } = catalog_stmt("CREATE GRAPH g { (:Person) }")
        else {
            panic!("not a create graph");
        };
        let GraphTypeRef::Source(GraphTypeSource::Elements(elements)) = of else {
            panic!("not a type written out");
        };
        assert_eq!(elements[0].labels, ["Person"]);
        assert_eq!(
            catalog_stmt("CREATE GRAPH g LIKE h"),
            CatalogStmt::CreateGraph {
                name: GraphName {
                    schema: None,
                    name: "g".into(),
                },
                if_not_exists: false,
                or_replace: false,
                of: GraphTypeRef::Source(GraphTypeSource::Like("h".into())),
                copy_of: None,
            }
        );
        assert_eq!(
            catalog_stmt("DROP PROPERTY GRAPH IF EXISTS g"),
            CatalogStmt::DropGraph {
                name: GraphName {
                    schema: None,
                    name: "g".into(),
                },
                if_exists: true,
            }
        );
    }

    #[test]
    fn a_catalog_statement_says_what_it_could_not_read() {
        assert!(
            catalog_err("DROP GRAPH TYPE IF NOT EXISTS t")
                .contains("the modifier here is IF EXISTS"),
            "the two modifiers are not interchangeable"
        );
        assert!(
            catalog_err("CREATE OR REPLACE GRAPH TYPE IF NOT EXISTS t { (:A) }")
                .contains("says nothing"),
            "taking the name over and leaving it alone are different answers"
        );
        assert!(
            catalog_err("CREATE OR REPLACE SCHEMA /app").contains("OR REPLACE is not written here")
        );
        assert!(
            catalog_err("CREATE SCHEMA app").contains("expected an absolute directory path"),
            "a schema is named by a path and not by a word"
        );
        assert!(catalog_err("CREATE GRAPH TYPE t { (: ) }").contains("expected a label"));
        assert!(catalog_err("CREATE GRAPH TYPE t { NODE (:A) }").contains("expected TYPE"));
        assert!(
            catalog_err("CREATE GRAPH TYPE t { (:A)<-[:R]->(:B) }")
                .contains("an arc with one arrowhead")
        );
        assert!(
            catalog_err("CREATE GRAPH TYPE t { (:A) } RETURN 1")
                .contains("nothing may follow a catalog statement")
        );
        assert!(
            parse_err("CREATE GRAPH TYPE t LIKE social").contains("runs through the session"),
            "the query path does not run a statement that writes"
        );
        // CREATE that is not a catalog statement still says what it is.
        assert!(parse_err("CREATE (n) RETURN n").contains("CREATE is not implemented yet"));
    }

    /// A statement GQL defines and the v0 core does not parse should be
    /// turned away by name. Being told the parser expected MATCH sends a
    /// reader looking for a typo in a statement they spelled correctly,
    /// which is the wrong place to look and the wrong thing to fix.
    #[test]
    fn a_statement_we_do_not_parse_yet_is_refused_by_name() {
        for (source, kw) in [
            ("SESSION SET VALUE $x = 1", "SESSION"),
            ("MATCH (p) FINISH", "FINISH"),
            ("MERGE (p:Person) RETURN p", "MERGE"),
        ] {
            let err = parse_err(source);
            assert!(
                err.contains(&format!("{kw} is not implemented yet")),
                "{source:?} was refused with {err:?}, which does not name {kw}"
            );
        }
    }

    #[test]
    fn a_transaction_starts_read_write_unless_it_says_otherwise() {
        assert_eq!(
            txn_stmt("START TRANSACTION"),
            TxnStmt::Start { read_only: false }
        );
        assert_eq!(
            txn_stmt("start transaction read only"),
            TxnStmt::Start { read_only: true }
        );
        assert_eq!(
            txn_stmt("START TRANSACTION READ WRITE"),
            TxnStmt::Start { read_only: false }
        );
        assert_eq!(txn_stmt("COMMIT"), TxnStmt::Commit);
        assert_eq!(txn_stmt("ROLLBACK;"), TxnStmt::Rollback);
    }

    #[test]
    fn a_transaction_statement_says_what_it_could_not_read() {
        assert!(
            catalog_err("START TRANSACTION READ ONLY, READ WRITE").contains("written both"),
            "a transaction that is both is neither"
        );
        assert!(
            catalog_err("START TRANSACTION MATCH (n) RETURN n")
                .contains("nothing may follow a transaction statement")
        );
        assert!(catalog_err("START READ ONLY").contains("expected TRANSACTION"));
        assert!(
            parse_err("COMMIT").contains("runs through the session"),
            "the query path does not run a statement that ends a transaction"
        );
    }

    /// ORDER BY, SKIP and LIMIT belong to the projection that precedes
    /// them, so reserving statement keywords at the head of a clause
    /// must not reach inside a RETURN that is parsing normally.
    #[test]
    fn reserving_statement_keywords_leaves_the_projection_alone() {
        for source in [
            "MATCH (n) RETURN n ORDER BY n.age",
            "MATCH (n) RETURN n ORDER BY n.age DESC LIMIT 3",
            "MATCH (n) WITH n ORDER BY n.age RETURN n",
        ] {
            parsed(source);
        }
    }

    /// The label expression on the first node of the first pattern.
    fn label_of(source: &str) -> LabelExpr {
        let q = parsed(source);
        let Clause::Match { patterns, .. } = q.clauses()[0] else {
            panic!("a MATCH");
        };
        patterns[0].start.label.clone().expect("a label")
    }

    fn label(name: &str) -> LabelExpr {
        LabelExpr::Label(name.into())
    }

    fn and(lhs: LabelExpr, rhs: LabelExpr) -> LabelExpr {
        LabelExpr::And(Box::new(lhs), Box::new(rhs))
    }

    fn or(lhs: LabelExpr, rhs: LabelExpr) -> LabelExpr {
        LabelExpr::Or(Box::new(lhs), Box::new(rhs))
    }

    #[test]
    fn a_label_expression_binds_or_loosest_and_not_tightest() {
        // A repeated colon is the conjunction Cypher writes, and `&`
        // is the same thing in GQL's own spelling.
        assert_eq!(
            label_of("MATCH (n:A:B) RETURN n"),
            and(label("A"), label("B"))
        );
        assert_eq!(
            label_of("MATCH (n:A&B) RETURN n"),
            and(label("A"), label("B"))
        );
        // `|` is looser than `&`, so this reads A or (B and C).
        assert_eq!(
            label_of("MATCH (n:A|B&C) RETURN n"),
            or(label("A"), and(label("B"), label("C")))
        );
        // Parentheses say the other grouping.
        assert_eq!(
            label_of("MATCH (n:(A|B)&C) RETURN n"),
            and(or(label("A"), label("B")), label("C"))
        );
        // `!` binds tighter than either, and stacks.
        assert_eq!(
            label_of("MATCH (n:!A&B) RETURN n"),
            and(LabelExpr::Not(Box::new(label("A"))), label("B"))
        );
        assert_eq!(
            label_of("MATCH (n:!!A) RETURN n"),
            LabelExpr::Not(Box::new(LabelExpr::Not(Box::new(label("A")))))
        );
        // `%` is a label expression of its own.
        assert_eq!(label_of("MATCH (n:%) RETURN n"), LabelExpr::Wildcard);
        assert_eq!(
            label_of("MATCH (n:!(A|B)) RETURN n"),
            LabelExpr::Not(Box::new(or(label("A"), label("B"))))
        );
        // And the operators still need something to work on.
        assert!(parse_err("MATCH (n:A&) RETURN n").contains("a label name"));
        assert!(parse_err("MATCH (n:(A|B) RETURN n").contains("')'"));
    }

    #[test]
    fn point_lookup_shape() {
        // LDBC short-read shape: one labeled node with a param prop.
        let q = parsed("MATCH (n:Person {id: $personId}) RETURN n.firstName AS firstName");
        assert_eq!(
            q.clauses().len(),
            1,
            "the MATCH, the RETURN being the result"
        );
        let Clause::Match {
            optional,
            patterns,
            filter,
        } = &q.clauses()[0]
        else {
            panic!("first clause is MATCH");
        };
        assert!(!optional && filter.is_none());
        let node = &patterns[0].start;
        assert_eq!(node.var.as_deref(), Some("n"));
        assert_eq!(node.label, Some(LabelExpr::Label("Person".into())));
        assert_eq!(node.props[0].0, "id");
        assert_eq!(node.props[0].1, Expr::Param("personId".into()));
        let projection = q.result().expect("RETURN");
        assert_eq!(projection.items[0].alias.as_deref(), Some("firstName"));
    }

    #[test]
    fn directions_and_hop_ranges() {
        let q =
            parsed("MATCH (a)-[:KNOWS*1..2]->(b), (a)<-[r:LIKES|FOLLOWS]-(c), (b)--(c) RETURN *");
        let Clause::Match { patterns, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        let (knows, _) = &patterns[0].steps[0];
        assert_eq!(knows.direction, RelDirection::Out);
        assert_eq!(knows.range, Some((Some(1), Some(2))));
        let (likes, _) = &patterns[1].steps[0];
        assert_eq!(likes.direction, RelDirection::In);
        assert_eq!(likes.var.as_deref(), Some("r"));
        assert_eq!(likes.types, ["LIKES", "FOLLOWS"]);
        let (bare, _) = &patterns[2].steps[0];
        assert_eq!(bare.direction, RelDirection::Any);
        assert!(bare.range.is_none());
        let projection = q.result().expect("RETURN");
        assert!(projection.star);
    }

    /// The seven edge patterns of ISO 39075 18.9, in full and in the
    /// abbreviated spellings that drop the bracket (GH02).
    #[test]
    fn seven_edge_patterns_and_their_abbreviations() {
        let full = [
            ("(a)-[r]->(b)", RelDirection::Out),
            ("(a)<-[r]-(b)", RelDirection::In),
            ("(a)<-[r]->(b)", RelDirection::AnyDirected),
            ("(a)~[r]~(b)", RelDirection::Undirected),
            ("(a)<~[r]~(b)", RelDirection::InOrUndirected),
            ("(a)~[r]~>(b)", RelDirection::OutOrUndirected),
            ("(a)-[r]-(b)", RelDirection::Any),
        ];
        let short = [
            ("(a)->(b)", RelDirection::Out),
            ("(a)<-(b)", RelDirection::In),
            ("(a)<->(b)", RelDirection::AnyDirected),
            ("(a)~(b)", RelDirection::Undirected),
            ("(a)<~(b)", RelDirection::InOrUndirected),
            ("(a)~>(b)", RelDirection::OutOrUndirected),
            ("(a)-(b)", RelDirection::Any),
            ("(a)--(b)", RelDirection::Any),
        ];
        for (pattern, want) in full.into_iter().chain(short) {
            let q = parsed(&format!("MATCH {pattern} RETURN a"));
            let Clause::Match { patterns, .. } = &q.clauses()[0] else {
                panic!("MATCH");
            };
            let (rel, _) = &patterns[0].steps[0];
            assert_eq!(rel.direction, want, "{pattern}");
        }
    }

    /// Which stored lists each pattern reads, and which tables it may
    /// read at all, is what the binder and the engine ask of a
    /// direction rather than matching on the variants themselves.
    #[test]
    fn a_pattern_resolves_against_the_table_it_walks() {
        use RelDirection::*;
        // A directed table keeps the arrows and refuses the tilde.
        assert_eq!(Out.resolve(false), Some(Out));
        assert_eq!(OutOrUndirected.resolve(false), Some(Out));
        assert_eq!(InOrUndirected.resolve(false), Some(In));
        assert_eq!(AnyDirected.resolve(false), Some(Any));
        assert_eq!(Undirected.resolve(false), None);
        // Either way round is still a direction, so it refuses an edge
        // that has none.
        assert_eq!(AnyDirected.resolve(true), None);
        // An undirected edge is stored once, so every pattern that
        // admits it reads both lists, and the arrow-only ones do not.
        assert_eq!(Undirected.resolve(true), Some(Any));
        assert_eq!(OutOrUndirected.resolve(true), Some(Any));
        assert_eq!(Any.resolve(true), Some(Any));
        assert_eq!(Out.resolve(true), None);
        assert_eq!(In.resolve(true), None);
        // And a flipped pattern is the same pattern read backwards.
        assert_eq!(OutOrUndirected.flip(), InOrUndirected);
        assert_eq!(Undirected.flip(), Undirected);
        assert_eq!(AnyDirected.flip(), AnyDirected);
        assert_eq!(Out.flip(), In);
    }

    #[test]
    fn path_modes_and_selectors_parse() {
        let q = parsed(
            "MATCH p = ANY SHORTEST TRAIL (a)-[:KNOWS*]->(b), \
             ALL SHORTEST (a)-[:KNOWS*]->(c), \
             WALK (a)-[:KNOWS*1..2]->(d), \
             ACYCLIC (a)-[:KNOWS*]->(e) RETURN *",
        );
        let Clause::Match { patterns, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        assert_eq!(patterns[0].var.as_deref(), Some("p"));
        assert_eq!(patterns[0].selector, Some(Selector::AnyShortest));
        assert_eq!(patterns[0].mode, Some(PathMode::Trail));
        assert_eq!(patterns[1].selector, Some(Selector::AllShortest));
        // The second names no mode, and walks under TRAIL because that
        // is the default rather than because it said so.
        assert_eq!(patterns[1].mode, None);
        assert_eq!(patterns[1].mode.unwrap_or_default(), PathMode::Trail);
        assert_eq!(patterns[2].selector, None);
        assert_eq!(patterns[2].mode, Some(PathMode::Walk));
        assert_eq!(patterns[3].mode, Some(PathMode::Acyclic));
    }

    /// `SHORTEST` on its own says shortest of how many, and there is no
    /// answer to read into it: `SHORTEST 1` and `ALL SHORTEST` are both
    /// shortest and are different questions.
    #[test]
    fn bare_shortest_reads_as_an_error() {
        let e = parse_err("MATCH SHORTEST (a)-[:KNOWS*]->(b) RETURN *");
        assert!(e.contains("needs a quantity"), "got: {e}");
    }

    /// All seven selectors of ISO 16.6, and the noise words the standard
    /// lets them carry. `ALL PATHS` is every path a pattern matches,
    /// which is what a pattern with no selector keeps, so it reads as
    /// none rather than as a selector of its own.
    #[test]
    fn every_path_selector_reads_as_itself() {
        for (source, want) in [
            ("ALL", None),
            ("ALL PATHS", None),
            ("ALL PATH", None),
            ("ANY", Some(Selector::Any(1))),
            ("ANY PATHS", Some(Selector::Any(1))),
            ("ANY 3", Some(Selector::Any(3))),
            ("ANY 3 PATHS", Some(Selector::Any(3))),
            ("ALL SHORTEST", Some(Selector::AllShortest)),
            ("ALL SHORTEST PATHS", Some(Selector::AllShortest)),
            ("ANY SHORTEST", Some(Selector::AnyShortest)),
            ("ANY SHORTEST PATH", Some(Selector::AnyShortest)),
            ("SHORTEST 2", Some(Selector::Shortest(2))),
            ("SHORTEST 2 PATHS", Some(Selector::Shortest(2))),
            ("SHORTEST 2 GROUP", Some(Selector::ShortestGroup(2))),
            ("SHORTEST 2 GROUPS", Some(Selector::ShortestGroup(2))),
        ] {
            let q = parsed(&format!("MATCH {source} (a)-[:KNOWS*1..3]->(b) RETURN *"));
            let Clause::Match { patterns, .. } = &q.clauses()[0] else {
                panic!("MATCH");
            };
            assert_eq!(patterns[0].selector, want, "{source}");
        }
    }

    /// The mode belongs inside the prefix, between the selector and the
    /// noise words, and `GROUP` comes after all of it. A group count
    /// left out means one group, which is the one case where `SHORTEST`
    /// stands without a number of its own.
    #[test]
    fn the_mode_sits_inside_the_selector_and_group_comes_last() {
        for (source, selector, mode) in [
            ("ALL TRAIL PATHS", None, Some(PathMode::Trail)),
            ("ALL WALK", None, Some(PathMode::Walk)),
            ("TRAIL PATHS", None, Some(PathMode::Trail)),
            (
                "ANY 2 ACYCLIC PATHS",
                Some(Selector::Any(2)),
                Some(PathMode::Acyclic),
            ),
            (
                "ANY SHORTEST SIMPLE PATH",
                Some(Selector::AnyShortest),
                Some(PathMode::Simple),
            ),
            (
                "ALL SHORTEST ACYCLIC PATHS",
                Some(Selector::AllShortest),
                Some(PathMode::Acyclic),
            ),
            (
                "SHORTEST 2 SIMPLE PATHS",
                Some(Selector::Shortest(2)),
                Some(PathMode::Simple),
            ),
            (
                "SHORTEST 2 ACYCLIC PATHS GROUPS",
                Some(Selector::ShortestGroup(2)),
                Some(PathMode::Acyclic),
            ),
            ("SHORTEST GROUPS", Some(Selector::ShortestGroup(1)), None),
            (
                "SHORTEST TRAIL GROUP",
                Some(Selector::ShortestGroup(1)),
                Some(PathMode::Trail),
            ),
        ] {
            let q = parsed(&format!("MATCH {source} (a)-[:KNOWS*1..3]->(b) RETURN *"));
            let Clause::Match { patterns, .. } = &q.clauses()[0] else {
                panic!("MATCH");
            };
            assert_eq!(patterns[0].selector, selector, "{source}");
            assert_eq!(patterns[0].mode, mode, "{source}");
        }
    }

    /// A KEEP says a prefix once for a whole list of patterns, so what
    /// each pattern of the list ends up carrying is what it wrote itself
    /// and, where it wrote nothing, what the KEEP wrote.
    #[test]
    fn a_keep_says_the_prefix_for_every_pattern_of_the_list() {
        let q = parsed(
            "MATCH (a)-[:KNOWS*1..3]->(b), WALK (b)-[:KNOWS*1..2]->(c) \
             KEEP ALL SHORTEST RETURN *",
        );
        let Clause::Match { patterns, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        // The first wrote nothing, so the selector is all it carries.
        assert_eq!(patterns[0].selector, Some(Selector::AllShortest));
        assert_eq!(patterns[0].mode, None);
        // The second wrote a mode, which it keeps, and takes the
        // selector beside it.
        assert_eq!(patterns[1].selector, Some(Selector::AllShortest));
        assert_eq!(patterns[1].mode, Some(PathMode::Walk));

        // And the other way round: a KEEP naming a mode leaves a
        // selector a pattern wrote alone.
        let q = parsed(
            "MATCH (a)-[:KNOWS*1..3]->(b), ANY (b)-[:KNOWS*1..2]->(c) \
             KEEP ACYCLIC PATHS RETURN *",
        );
        let Clause::Match { patterns, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        assert_eq!(patterns[0].selector, None);
        assert_eq!(patterns[0].mode, Some(PathMode::Acyclic));
        assert_eq!(patterns[1].selector, Some(Selector::Any(1)));
        assert_eq!(patterns[1].mode, Some(PathMode::Acyclic));
    }

    /// A KEEP naming what a pattern has already named is refused. Either
    /// answer, the pattern's or the KEEP's, throws away something the
    /// query asked for, and a query asking for two contradictory things
    /// is a query somebody wrote by mistake.
    #[test]
    fn a_keep_that_names_what_a_pattern_named_is_refused() {
        for (source, want) in [
            (
                "MATCH ANY SHORTEST (a)-[:KNOWS*]->(b) KEEP ALL SHORTEST RETURN *",
                "carries a path selector",
            ),
            (
                "MATCH WALK (a)-[:KNOWS*1..2]->(b) KEEP ACYCLIC RETURN *",
                "carries a path mode",
            ),
            (
                "MATCH (a)-[:KNOWS*1..2]->(b) KEEP RETURN *",
                "needs a path selector or a path mode",
            ),
        ] {
            let e = parse_err(source);
            assert!(e.contains(want), "{source}: {e}");
        }
    }

    /// A selector that keeps no path answers nothing whatever the graph
    /// holds, so the count is refused rather than obeyed.
    #[test]
    fn a_path_count_of_zero_is_refused() {
        for source in ["ANY 0", "SHORTEST 0", "SHORTEST 0 GROUPS"] {
            let e = parse_err(&format!("MATCH {source} (a)-[:KNOWS*]->(b) RETURN *"));
            assert!(e.contains("keeps no path at all"), "{source}: {e}");
        }
    }

    /// SIMPLE is a mode of its own and used to be turned away with a
    /// suggestion to write ACYCLIC, which forbids a different set of
    /// paths, so the two do not read as the same thing here either.
    #[test]
    fn simple_is_its_own_mode() {
        let q = parsed("MATCH SIMPLE (a)-[:KNOWS*]->(b) RETURN *");
        let Clause::Match { patterns, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        assert_eq!(patterns[0].mode, Some(PathMode::Simple));
    }

    #[test]
    fn star_ranges_cover_every_form() {
        for (text, want) in [
            ("*", (None, None)),
            ("*3", (Some(3), Some(3))),
            ("*1..4", (Some(1), Some(4))),
            ("*..4", (None, Some(4))),
            ("*2..", (Some(2), None)),
        ] {
            let q = parsed(&format!("MATCH (a)-[{text}]->(b) RETURN a"));
            let Clause::Match { patterns, .. } = &q.clauses()[0] else {
                panic!("MATCH");
            };
            assert_eq!(patterns[0].steps[0].0.range, Some(want), "range {text}");
        }
    }

    /// The other way of writing the same repetition, the standard's
    /// own: a quantifier behind the arrow. Every form lands in the
    /// range the brackets would have carried, so nothing past the
    /// parser can tell which way a step was written.
    #[test]
    fn a_quantifier_behind_the_arrow_reads_as_a_range() {
        for (text, want) in [
            ("-[:KNOWS]->+", (Some(1), None)),
            ("-[:KNOWS]->*", (Some(0), None)),
            ("-[:KNOWS]->{3}", (Some(3), Some(3))),
            ("-[:KNOWS]->{2,}", (Some(2), None)),
            ("-[:KNOWS]->{,4}", (None, Some(4))),
            ("-[:KNOWS]->{2,4}", (Some(2), Some(4))),
            // The abbreviated edge pattern takes one too, and so does
            // an edge pointing the other way.
            ("-->+", (Some(1), None)),
            ("<-[:KNOWS]-{2,4}", (Some(2), Some(4))),
        ] {
            let q = parsed(&format!("MATCH (a){text}(b) RETURN a"));
            let Clause::Match { patterns, .. } = &q.clauses()[0] else {
                panic!("MATCH");
            };
            assert_eq!(
                patterns[0].steps[0].0.range,
                Some(want),
                "quantifier {text}"
            );
        }
    }

    /// Two quantities on one step is a question rather than an
    /// instruction, and a quantifier that names no number at all says
    /// nothing.
    #[test]
    fn a_step_carries_one_quantity() {
        assert!(
            parse_err("MATCH (a)-[:KNOWS*2]->+(b) RETURN a").contains("not both"),
            "both forms at once"
        );
        assert!(
            parse_err("MATCH (a)-[:KNOWS]->{}(b) RETURN a").contains("says how many"),
            "an empty quantifier"
        );
    }

    /// A `WHERE` inside the brackets belongs to the step, and it reads
    /// after the type, the range and the property map, which are the
    /// three things that can precede it in there.
    #[test]
    fn a_where_inside_the_brackets_belongs_to_the_step() {
        for text in [
            "t:transfer WHERE t.ts >= 5",
            "t:transfer*1..3 WHERE t.ts >= 5",
            "t:transfer*1..3 {kind: 'wire'} WHERE t.ts >= 5",
        ] {
            let q = parsed(&format!("MATCH (a)-[{text}]->(b) RETURN a"));
            let Clause::Match { patterns, .. } = &q.clauses()[0] else {
                panic!("MATCH");
            };
            let (rel, _) = &patterns[0].steps[0];
            let Some(filter) = &rel.filter else {
                panic!("no predicate on {text}");
            };
            assert!(
                matches!(
                    **filter,
                    Expr::Binary {
                        op: BinaryOp::Ge,
                        ..
                    }
                ),
                "predicate of {text} parsed as {filter:?}"
            );
        }

        // Without one the step carries none, so nothing downstream has
        // to tell an absent predicate from one that is always true.
        let q = parsed("MATCH (a)-[t:transfer*1..3]->(b) RETURN a");
        let Clause::Match { patterns, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        assert!(patterns[0].steps[0].0.filter.is_none());
    }

    /// The same predicate on a node, which is where G041 writes it, and
    /// it reads after the labels and the property map the way the edge
    /// one reads after the type and the range.
    #[test]
    fn a_where_inside_the_parentheses_belongs_to_the_node() {
        for text in [
            "b WHERE b.step > 1",
            "b:Step WHERE b.step > 1",
            "b:Step {kind: 'link'} WHERE b.step > 1",
        ] {
            let q = parsed(&format!("MATCH (a)-[:LINK]->({text}) RETURN a"));
            let Clause::Match { patterns, .. } = &q.clauses()[0] else {
                panic!("MATCH");
            };
            let (_, node) = &patterns[0].steps[0];
            let Some(filter) = &node.filter else {
                panic!("no predicate on {text}");
            };
            assert!(
                matches!(
                    **filter,
                    Expr::Binary {
                        op: BinaryOp::Gt,
                        ..
                    }
                ),
                "predicate of {text} parsed as {filter:?}"
            );
        }

        // The one written on the node the pattern starts at is the same
        // predicate in the same place, and a node without one carries
        // none.
        let q = parsed("MATCH (a:Step WHERE a.step = 0)-[:LINK]->(b) RETURN b");
        let Clause::Match { patterns, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        assert!(patterns[0].start.filter.is_some());
        assert!(patterns[0].steps[0].1.filter.is_none());

        // Two stretches meeting at a node describe one node, so both
        // conditions written there are asked of it.
        let q =
            parsed("MATCH (a:Step WHERE a.step = 0)((a WHERE a.kind = 'x')-[:LINK]->(c)) RETURN c");
        let Clause::Match { patterns, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        let Some(filter) = &patterns[0].start.filter else {
            panic!("the two conditions met");
        };
        assert!(matches!(
            **filter,
            Expr::Binary {
                op: BinaryOp::And,
                ..
            }
        ));

        // Inside a repeated stretch it would be asked once per
        // repetition of an element the name no longer stands for, which
        // is refused by name rather than answered about the group.
        assert!(
            parse_err("MATCH ((x:Step WHERE x.step > 0)-[:LINK]->(y)){2} RETURN x")
                .contains("once per repetition"),
            "a predicate inside a repeated stretch"
        );
    }

    /// GE01. The word after CASE is what settles which form this is,
    /// and the two abbreviations are read where a call would be read
    /// without becoming calls.
    #[test]
    fn case_reads_both_forms_and_the_two_abbreviations() {
        let searched = parsed("RETURN CASE WHEN a > 1 THEN 'many' ELSE 'one' END AS n");
        let Expr::Case {
            subject,
            branches,
            otherwise,
        } = &searched.result().expect("RETURN").items[0].expr
        else {
            panic!("the searched form");
        };
        assert!(subject.is_none(), "no value before the first WHEN");
        assert_eq!(branches.len(), 1);
        assert!(otherwise.is_some());

        let simple = parsed("RETURN CASE a.kind WHEN 'x' THEN 1 WHEN 'y' THEN 2 END AS n");
        let Expr::Case {
            subject,
            branches,
            otherwise,
        } = &simple.result().expect("RETURN").items[0].expr
        else {
            panic!("the simple form");
        };
        assert!(subject.is_some(), "the value each branch is compared with");
        assert_eq!(branches.len(), 2);
        assert!(otherwise.is_none(), "an ELSE nobody wrote");

        let abbreviated = parsed("RETURN COALESCE(a, b, 0) AS n, NULLIF(a, b) AS m");
        let items = &abbreviated.result().expect("RETURN").items;
        let Expr::Coalesce(args) = &items[0].expr else {
            panic!("COALESCE");
        };
        assert_eq!(args.len(), 3);
        assert!(matches!(items[1].expr, Expr::NullIf { .. }));

        // A CASE with no branch answers nothing, and NULLIF asks about
        // two values however many the query wrote.
        assert!(parse_err("RETURN CASE ELSE 1 END AS n").contains("WHEN"));
        assert!(parse_err("RETURN NULLIF(a) AS n").contains("two arguments"));
    }

    #[test]
    fn with_aggregation_pipeline() {
        let q = parsed(
            "MATCH (a:Person)-[:KNOWS]->(b) WITH a, count(b) AS friends WHERE friends > 5 \
             RETURN DISTINCT a.name, friends ORDER BY friends DESC, a.name SKIP 2 LIMIT 10",
        );
        let Clause::With { projection, filter } = &q.clauses()[1] else {
            panic!("WITH");
        };
        assert!(filter.is_some());
        let Expr::Call { name, star, .. } = &projection.items[1].expr else {
            panic!("count call");
        };
        assert_eq!(name, "count");
        assert!(!star);
        let projection = q.result().expect("RETURN");
        assert!(projection.distinct);
        assert_eq!(projection.order_by.len(), 2);
        assert!(!projection.order_by[0].ascending, "DESC");
        assert!(projection.order_by[1].ascending, "implicit ASC");
        // A key that says nothing about its nulls gets the implicit
        // ordering, which zu documents as last.
        assert!(
            projection
                .order_by
                .iter()
                .all(|key| key.nulls == NullOrder::Last)
        );
        assert_eq!(projection.skip, Some(Expr::Literal(Literal::Int(2))));
        assert_eq!(projection.limit, Some(Expr::Literal(Literal::Int(10))));
    }

    #[test]
    fn a_sort_key_says_where_its_nulls_go() {
        let keys = |tail: &str| {
            let q = parsed(&format!("MATCH (a) RETURN a.x AS x ORDER BY {tail}"));
            let projection = q.result().expect("RETURN");
            projection.order_by.clone()
        };
        // The two halves are independent, so all four pairings parse
        // and neither half reads the other.
        for (tail, ascending, nulls) in [
            ("x", true, NullOrder::Last),
            ("x NULLS FIRST", true, NullOrder::First),
            ("x NULLS LAST", true, NullOrder::Last),
            ("x DESC NULLS FIRST", false, NullOrder::First),
            ("x DESCENDING NULLS LAST", false, NullOrder::Last),
            ("x ASC NULLS FIRST", true, NullOrder::First),
        ] {
            let got = keys(tail);
            assert_eq!(got.len(), 1, "{tail}");
            assert_eq!(got[0].ascending, ascending, "{tail}");
            assert_eq!(got[0].nulls, nulls, "{tail}");
        }
        // Each key in a list answers for itself.
        let two = keys("x NULLS FIRST, a.y DESC");
        assert_eq!(two[0].nulls, NullOrder::First);
        assert_eq!(two[1].nulls, NullOrder::Last);
        assert!(!two[1].ascending);
        // NULLS is only a keyword in front of FIRST or LAST, and the
        // word it wants is the one the error names.
        let err = parse_err("MATCH (a) RETURN a.x AS x ORDER BY x NULLS SOMEWHERE");
        assert!(err.contains("LAST"), "unexpected error: {err}");
    }

    #[test]
    fn unwind_and_lists() {
        let q = parsed("UNWIND [1, 2, 3] AS x RETURN x * -1");
        let Clause::Unwind { expr, alias, .. } = &q.clauses()[0] else {
            panic!("UNWIND");
        };
        assert_eq!(alias, "x");
        assert_eq!(
            *expr,
            Expr::List(vec![
                Expr::Literal(Literal::Int(1)),
                Expr::Literal(Literal::Int(2)),
                Expr::Literal(Literal::Int(3)),
            ])
        );
        let projection = q.result().expect("RETURN");
        let Expr::Binary {
            op: BinaryOp::Mul,
            rhs,
            ..
        } = &projection.items[0].expr
        else {
            panic!("multiply");
        };
        assert_eq!(**rhs, Expr::Literal(Literal::Int(-1)));
    }

    #[test]
    fn call_parses_args_and_yield_aliases() {
        let q = parsed("CALL sssp('KNOWS', 42) YIELD node AS n, distance RETURN n, distance");
        let Clause::Call { name, args, yields } = &q.clauses()[0] else {
            panic!("CALL");
        };
        assert_eq!(name, "sssp");
        assert_eq!(
            *args,
            vec![
                Expr::Literal(Literal::Str("KNOWS".into())),
                Expr::Literal(Literal::Int(42)),
            ]
        );
        assert_eq!(
            *yields,
            vec![
                ("node".to_string(), Some("n".to_string())),
                ("distance".to_string(), None),
            ]
        );
    }

    #[test]
    fn call_without_yield_is_an_error() {
        let err = parse("CALL pagerank('KNOWS') RETURN 1").unwrap_err();
        assert!(err.to_string().contains("YIELD"), "{err}");
    }

    #[test]
    fn operator_precedence_and_string_predicates() {
        let q = parsed(
            "MATCH (n) WHERE NOT n.age + 1 * 2 >= 10 AND n.name STARTS WITH 'A' \
             OR n.id IN [1, 2] AND n.bio IS NOT NULL RETURN n",
        );
        let Clause::Match {
            filter: Some(filter),
            ..
        } = &q.clauses()[0]
        else {
            panic!("WHERE");
        };
        // OR is the loosest binder: (NOT(...) AND starts) OR (in AND is-not-null).
        let Expr::Binary {
            op: BinaryOp::Or,
            lhs,
            rhs,
        } = filter
        else {
            panic!("OR at the top, got {filter:?}");
        };
        let Expr::Binary {
            op: BinaryOp::And,
            lhs: not_side,
            ..
        } = &**lhs
        else {
            panic!("AND under OR");
        };
        assert!(matches!(
            &**not_side,
            Expr::Unary {
                op: UnaryOp::Not,
                ..
            }
        ));
        let Expr::Binary {
            op: BinaryOp::And,
            rhs: null_side,
            ..
        } = &**rhs
        else {
            panic!("AND on the right");
        };
        assert!(matches!(&**null_side, Expr::IsNull { negated: true, .. }));
    }

    #[test]
    fn concatenation_sits_between_the_comparisons_and_the_additions() {
        // ISO 20.23. The join is under the equals and the sum is under
        // the join, so the query asks whether the joined string equals
        // the one on the right, and the right hand operand of the join
        // is the sum rather than the one.
        let q = parsed("MATCH (n) WHERE n.a || 1 + 2 = n.b RETURN n");
        let Clause::Match {
            filter: Some(filter),
            ..
        } = &q.clauses()[0]
        else {
            panic!("WHERE");
        };
        let Expr::Binary {
            op: BinaryOp::Eq,
            lhs,
            ..
        } = filter
        else {
            panic!("= at the top, got {filter:?}");
        };
        let Expr::Binary {
            op: BinaryOp::Concat,
            rhs: sum,
            ..
        } = &**lhs
        else {
            panic!("|| under the =, got {lhs:?}");
        };
        assert!(matches!(
            &**sum,
            Expr::Binary {
                op: BinaryOp::Add,
                ..
            }
        ));

        // And it folds to the left the way the other binary operators
        // do, so three strings joined are two joins and not a list.
        let q = parsed("RETURN 'a' || 'b' || 'c' AS v");
        let projection = q.result().expect("RETURN");
        let Expr::Binary {
            op: BinaryOp::Concat,
            lhs,
            ..
        } = &projection.items[0].expr
        else {
            panic!("|| at the top");
        };
        assert!(matches!(
            &**lhs,
            Expr::Binary {
                op: BinaryOp::Concat,
                ..
            }
        ));
    }

    #[test]
    fn optional_match_and_path_binding() {
        let q =
            parsed("MATCH p = (a)-[:KNOWS]->(b) OPTIONAL MATCH (b)-[:WORKS_AT]->(c) RETURN p, c");
        let Clause::Match {
            optional, patterns, ..
        } = &q.clauses()[0]
        else {
            panic!("MATCH");
        };
        assert!(!optional);
        assert_eq!(patterns[0].var.as_deref(), Some("p"));
        let Clause::Match { optional, .. } = &q.clauses()[1] else {
            panic!("OPTIONAL MATCH");
        };
        assert!(optional);
    }

    #[test]
    fn errors_name_position_and_expectation() {
        assert!(
            parse_err("MATCH (n RETURN n").contains("expected ')'"),
            "unclosed node"
        );
        assert!(
            parse_err("MATCH (n)")
                .contains("expected MATCH, OPTIONAL MATCH, CALL, UNWIND, WITH, or RETURN")
        );
        assert!(parse_err("RETURN 1 RETURN 2").contains("nothing may follow RETURN"));
        assert!(parse_err("").contains("empty query"));
        assert!(parse_err("CREATE (n) RETURN n").contains("CREATE is not implemented yet"));
        assert!(
            parse_err("MATCH (a)<~[r]~>(b) RETURN a")
                .contains("an undirected relationship cannot point both ways")
        );
        assert!(
            parse_err("MATCH (a)<-[r]~(b) RETURN a")
                .contains("undirected at both ends or at neither")
        );
        let e = parse_err("MATCH (n)\nWHERE n.x =\nRETURN n");
        assert!(
            e.contains("line 3"),
            "position points at the missing operand: {e}"
        );
    }

    #[test]
    fn hostile_nesting_errors_instead_of_overflowing() {
        let mut source = String::from("RETURN ");
        for _ in 0..5000 {
            source.push('(');
        }
        source.push('1');
        for _ in 0..5000 {
            source.push(')');
        }
        assert!(
            parse(&source)
                .expect_err("too deep")
                .to_string()
                .contains("nesting")
        );
        // Deep NOT and minus chains stay iterative, so they parse.
        let nots = "NOT ".repeat(5000);
        parse(&format!("MATCH (n) WHERE {nots}true RETURN n")).expect("NOT chain parses");
        let minuses = "-".repeat(5000);
        parse(&format!("RETURN {minuses}1")).expect("minus chain parses");
    }

    #[test]
    fn exists_blocks_parse() {
        let q = parsed(
            "MATCH (a:Person) \
             WHERE EXISTS { MATCH (a)-[:KNOWS]->(b), (b)-[:KNOWS]->(c) WHERE c.id > 3 } \
             RETURN a.id AS id",
        );
        let Clause::Match { filter, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        let Some(Expr::Exists { patterns, filter }) = filter else {
            panic!("the WHERE is the block itself, got {filter:?}");
        };
        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[1].start.var.as_deref(), Some("b"));
        assert!(filter.is_some(), "the block's own WHERE came with it");
    }

    /// GQ22. Several MATCH statements in one block are one conjunction,
    /// so the patterns gather and the conditions fold together with AND.
    #[test]
    fn a_block_may_hold_several_match_statements() {
        let q = parsed(
            "MATCH (a:Person) \
             WHERE EXISTS { MATCH (a)-[:KNOWS]->(b) WHERE b.id > 1 \
                            MATCH (b)-[:KNOWS]->(c) WHERE c.id > 3 } \
             RETURN a.id AS id",
        );
        let Clause::Match { filter, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        let Some(Expr::Exists { patterns, filter }) = filter else {
            panic!("the WHERE is the block itself, got {filter:?}");
        };
        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[1].start.var.as_deref(), Some("b"));
        let Some(inner) = filter else {
            panic!("both conditions came with it");
        };
        assert!(
            matches!(
                inner.as_ref(),
                Expr::Binary {
                    op: BinaryOp::And,
                    ..
                }
            ),
            "the two WHEREs fold into one condition, got {inner:?}"
        );
    }

    /// GQ19. A yield belongs to the match in front of it and says
    /// which of the names that match wrote leave it, so it reads as a
    /// clause of its own straight behind the match.
    #[test]
    fn a_match_may_yield_some_of_what_it_wrote() {
        let q = parsed("MATCH (a:Person)-[:KNOWS]->(b) YIELD b, a AS friend RETURN b.id AS id");
        let Clause::Yield { items } = &q.clauses()[1] else {
            panic!("the yield is a clause of its own, got {:?}", q.clauses()[1]);
        };
        assert_eq!(items[0].name, "b");
        assert_eq!(items[0].alias, None);
        assert_eq!(items[1].name, "a");
        assert_eq!(items[1].alias.as_deref(), Some("friend"));
        // The word is a table function's too, and that reading is the
        // one a CALL still gets.
        let q = parsed("CALL pagerank(0.85) YIELD score RETURN score AS s");
        assert!(
            matches!(&q.clauses()[0], Clause::Call { yields, .. } if yields.len() == 1),
            "a CALL yields its columns, got {:?}",
            q.clauses()[0]
        );
    }

    /// GQ18. A value query expression carries a whole query: it may
    /// chain, sort and cut, and `value` is still a variable name where
    /// no brace follows it.
    #[test]
    fn a_value_query_carries_a_whole_query() {
        let q = parsed("RETURN VALUE { MATCH (p:Person) RETURN COUNT(*) } AS total");
        let items = &q.result().expect("a RETURN").items;
        let Expr::ValueQuery(inner) = &items[0].expr else {
            panic!("the item is a value query, got {:?}", items[0].expr);
        };
        assert_eq!(inner.clauses().len(), 1, "the MATCH inside it");
        assert!(inner.result().is_some(), "and the RETURN it ends with");
        // A query and not a block: what may follow the RETURN inside is
        // what may follow any other one.
        let q = parsed("RETURN VALUE { MATCH (p:Person) RETURN p.id ORDER BY p.id LIMIT 1 } AS v");
        let items = &q.result().expect("a RETURN").items;
        assert!(matches!(&items[0].expr, Expr::ValueQuery(_)));
        // The word is free everywhere a brace does not follow it.
        let q = parsed("MATCH (value:Person) RETURN value.id AS id");
        assert_eq!(q.result().expect("a RETURN").items.len(), 1);
    }

    /// GQ21. An OPTIONAL takes a block as well as a single statement,
    /// and the block is one clause rather than one clause per statement.
    #[test]
    fn optional_takes_a_block_of_match_statements() {
        let q = parsed(
            "MATCH (a:Person) \
             OPTIONAL { MATCH (a)-[:KNOWS]->(b) MATCH (b)-[:KNOWS]->(c) } \
             RETURN a.id AS id",
        );
        assert_eq!(q.clauses().len(), 2, "one clause for the whole block");
        let Clause::Match {
            optional, patterns, ..
        } = &q.clauses()[1]
        else {
            panic!("the block is a match");
        };
        assert!(optional, "the block came from an OPTIONAL");
        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[1].start.var.as_deref(), Some("b"));
    }

    #[test]
    fn exists_takes_a_bare_pattern_and_a_not() {
        // MATCH inside the braces is a courtesy to the reader, and NOT
        // in front is an ordinary unary over the block.
        let q = parsed("MATCH (a) WHERE NOT EXISTS { (a)-[:KNOWS]->(b) } RETURN a");
        let Clause::Match { filter, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        let Some(Expr::Unary {
            op: UnaryOp::Not,
            expr,
        }) = filter
        else {
            panic!("NOT over the block, got {filter:?}");
        };
        let Expr::Exists { patterns, filter } = expr.as_ref() else {
            panic!("EXISTS under the NOT");
        };
        assert_eq!(patterns.len(), 1);
        assert!(filter.is_none());
    }

    #[test]
    fn exists_is_still_a_name_without_a_block() {
        // Only an opening bracket makes it the predicate, so a variable
        // of that name reads the way it always did.
        let q = parsed("MATCH (exists:Person) RETURN exists.id AS id");
        let Clause::Match { patterns, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        assert_eq!(patterns[0].start.var.as_deref(), Some("exists"));
    }

    #[test]
    fn exists_reads_all_three_of_the_shapes_it_is_written_in() {
        // A pattern, a block of matches and a whole query, the first
        // two of them in either brackets and the third in braces
        // only, which is ISO 19.4 read straight off.
        for source in [
            "MATCH (a) WHERE EXISTS { (a)-[:KNOWS]->(b) } RETURN a",
            "MATCH (a) WHERE EXISTS ( (a)-[:KNOWS]->(b) ) RETURN a",
            "MATCH (a) WHERE EXISTS { MATCH (a)-[:KNOWS]->(b) MATCH (b)-[:KNOWS]->(c) } RETURN a",
            "MATCH (a) WHERE EXISTS ( MATCH (a)-[:KNOWS]->(b) MATCH (b)-[:KNOWS]->(c) ) RETURN a",
        ] {
            let q = parsed(source);
            let Clause::Match { filter, .. } = &q.clauses()[0] else {
                panic!("MATCH");
            };
            assert!(
                matches!(filter, Some(Expr::Exists { .. })),
                "the block form, got {filter:?} from {source}"
            );
        }
        let q = parsed("MATCH (a) WHERE EXISTS { MATCH (b) RETURN b } RETURN a");
        let Clause::Match { filter, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        assert!(matches!(filter, Some(Expr::ExistsQuery(_))), "{filter:?}");
    }

    #[test]
    fn exists_refuses_what_a_predicate_cannot_use() {
        // A block is not a query and takes no clause that pages or
        // orders one, and parentheses hold a block and never a query,
        // which is the standard's rule and not this parser's.
        assert!(
            parse_err("MATCH (a) WHERE EXISTS { MATCH (a)-[:KNOWS]->(b) LIMIT 1 } RETURN a")
                .contains("expected")
        );
        assert!(
            parse_err("MATCH (a) WHERE EXISTS ( MATCH (b) RETURN b ) RETURN a")
                .contains("expected")
        );
    }

    /// The statements of a chain are statements, not clauses in a row:
    /// each one keeps the result it ends with, and what the reader
    /// wrote is what the tree says.
    #[test]
    fn next_chains_two_statements() {
        let q = parsed(
            "MATCH (n:Person) RETURN n AS p NEXT MATCH (p)-[:KNOWS]->(f:Person) RETURN f AS friend",
        );
        let linear = linear_body(&q);
        assert_eq!(linear.statements.len(), 2);
        assert_eq!(linear.statements[0].clauses.len(), 1, "the MATCH");
        assert_eq!(
            linear.statements[0]
                .result
                .as_ref()
                .expect("the first RETURN")
                .items[0]
                .alias
                .as_deref(),
            Some("p")
        );
        assert_eq!(linear.statements[1].clauses.len(), 1, "the second MATCH");
        assert_eq!(
            q.result().expect("the last RETURN").items[0]
                .alias
                .as_deref(),
            Some("friend"),
            "the query answers what the statement at the end of the chain answers"
        );
        assert_eq!(q.clauses().len(), 2, "one MATCH from each statement");
    }

    /// A chain is as long as it is written. Three statements is the
    /// shape the fused plan is measured on, so the parser has to hold
    /// three.
    #[test]
    fn a_chain_is_as_long_as_it_is_written() {
        let q = parsed(
            "MATCH (a:Person) RETURN a AS a NEXT MATCH (a)-[:KNOWS]->(b:Person) RETURN b AS b \
             NEXT MATCH (b)-[:IS_LOCATED_IN]->(c:Place) RETURN c.name AS name",
        );
        let linear = linear_body(&q);
        assert_eq!(linear.statements.len(), 3);
    }

    /// A write hands the chain the rows it was given, so it may stand
    /// in front of a NEXT without projecting anything.
    #[test]
    fn a_write_may_stand_in_front_of_next_without_returning() {
        let q = parsed("INSERT (x:Person) NEXT MATCH (n:Person) RETURN n");
        let linear = linear_body(&q);
        assert_eq!(linear.statements.len(), 2);
        assert!(
            linear.statements[0].result.is_none(),
            "the INSERT projected nothing"
        );
    }

    /// NEXT reads a result, so a read statement that produced none is
    /// told that rather than being told which clauses could follow.
    #[test]
    fn a_read_in_front_of_next_has_to_return() {
        let err = parse_err("MATCH (n:Person) NEXT MATCH (n) RETURN n");
        assert!(
            err.contains("NEXT reads what the statement in front of it returned"),
            "{err}"
        );
    }

    /// Everything a chain may not be followed by, it is still not
    /// followed by: the ending is checked once the chain has ended
    /// rather than at every RETURN in it.
    #[test]
    fn nothing_may_follow_the_end_of_a_chain() {
        let err = parse_err("MATCH (n:Person) RETURN n MATCH (m:Person) RETURN m");
        assert!(err.contains("nothing may follow RETURN"), "{err}");
        let err = parse_err("INSERT (x:Person); INSERT (y:Person)");
        assert!(
            err.contains("nothing may follow the end of a statement"),
            "{err}"
        );
    }

    /// A FILTER is a statement of its own, so it stands where a clause
    /// stands and takes the standard's optional WHERE without meaning
    /// anything different by it.
    #[test]
    fn filter_takes_its_condition_with_or_without_where() {
        for source in [
            "MATCH (n:Person) FILTER n.age > 30 RETURN n",
            "MATCH (n:Person) FILTER WHERE n.age > 30 RETURN n",
        ] {
            let q = parsed(source);
            let clauses = q.clauses();
            assert_eq!(clauses.len(), 2, "the MATCH and the FILTER");
            let Clause::Filter { expr } = clauses[1] else {
                panic!("the second clause is the FILTER");
            };
            assert!(matches!(expr, Expr::Binary { .. }), "{expr:?}");
        }
    }

    /// A LET is a list of definitions, name first, and each name is a
    /// variable rather than anything a projection item may be.
    #[test]
    fn let_reads_a_list_of_definitions() {
        let q = parsed("MATCH (n:Person) LET a = n.age, b = a + 1 RETURN b");
        let clauses = q.clauses();
        let Clause::Let { items } = clauses[1] else {
            panic!("the second clause is the LET");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "a");
        assert_eq!(items[1].name, "b");
    }

    /// `LET p.age = 30` is a write written where a definition goes, and
    /// the error names the statement that does it rather than reporting
    /// something unexpected at the dot.
    #[test]
    fn a_let_of_a_property_names_the_statement_that_writes() {
        let err = parse_err("MATCH (n:Person) LET n.age = 30 RETURN n");
        assert!(
            err.contains("changing a property of an element is SET"),
            "{err}"
        );
    }

    /// A FOR names the value in front of the list and may number the
    /// rows it makes, from one with ORDINALITY and from zero with
    /// OFFSET.
    #[test]
    fn for_names_its_value_and_may_count() {
        let q = parsed("FOR x IN [1, 2] RETURN x");
        let Clause::Unwind {
            alias,
            ordinal: None,
            ..
        } = &q.clauses()[0]
        else {
            panic!("a FOR with no counter");
        };
        assert_eq!(alias, "x");
        for (source, start) in [("WITH ORDINALITY i", 1), ("WITH OFFSET i", 0)] {
            let q = parsed(&format!("FOR x IN [1, 2] {source} RETURN x"));
            let Clause::Unwind {
                ordinal: Some(ordinal),
                ..
            } = &q.clauses()[0]
            else {
                panic!("a FOR with a counter: {source}");
            };
            assert_eq!(ordinal.name, "i");
            assert_eq!(ordinal.start, start, "{source}");
        }
    }

    /// WITH is a clause as well as the first word of a counter, and only
    /// the word after it says which was written, so a projection after a
    /// FOR is read as one.
    #[test]
    fn a_with_after_for_is_read_by_the_word_after_it() {
        let q = parsed("FOR x IN [1, 2] WITH x AS y RETURN y");
        let clauses = q.clauses();
        assert!(
            matches!(clauses[0], Clause::Unwind { ordinal: None, .. }),
            "the FOR takes no counter"
        );
        assert!(
            matches!(clauses[1], Clause::With { .. }),
            "the WITH is the projection it looks like"
        );
    }

    /// OFFSET is the standard's word for the clause Cypher spells SKIP,
    /// so the two parse to one field and writing both is writing the
    /// clause twice.
    #[test]
    fn offset_is_the_standard_spelling_of_skip() {
        for word in ["OFFSET", "SKIP"] {
            let q = parsed(&format!(
                "MATCH (a) RETURN a.x AS x ORDER BY x {word} 2 LIMIT 3"
            ));
            let projection = q.result().expect("RETURN");
            assert_eq!(
                projection.skip,
                Some(Expr::Literal(Literal::Int(2))),
                "{word} is the page's first clause"
            );
            assert_eq!(projection.limit, Some(Expr::Literal(Literal::Int(3))));
        }
        let err = parse_err("MATCH (a) RETURN a.x AS x ORDER BY x OFFSET 1 SKIP 1");
        assert!(err.contains("two spellings of one clause"), "{err}");
    }

    /// The conjunctions are all at one level and read left to right, so
    /// three operands nest to the left however the words are mixed. A
    /// parser that gave UNION and INTERSECT a precedence between them,
    /// as SQL does, would nest the last two together instead.
    #[test]
    fn conjunctions_fold_to_the_left() {
        let q = parsed(
            "MATCH (a:Person) RETURN a AS x \
             UNION ALL \
             MATCH (b:Person) RETURN b AS x \
             INTERSECT \
             MATCH (c:Person) RETURN c AS x",
        );
        let Composite::Conjoined { left, how, right } = &q.body else {
            panic!("two conjunctions, so a conjoined body");
        };
        assert_eq!(
            *how,
            Conjunction::Set {
                op: SetOp::Intersect,
                all: false
            },
            "the outermost conjunction is the one written last"
        );
        assert_eq!(right.statements.len(), 1);
        let Composite::Conjoined { how, .. } = left.as_ref() else {
            panic!("the first two operands are joined underneath");
        };
        assert_eq!(
            *how,
            Conjunction::Set {
                op: SetOp::Union,
                all: true
            }
        );
        assert_eq!(q.clauses().len(), 3, "one MATCH from each operand");
    }

    /// The set quantifier is optional and DISTINCT is what leaving it
    /// out means, which is the standard's default and the opposite of
    /// what a reader coming from a bag oriented language expects.
    #[test]
    fn a_missing_set_quantifier_reads_as_distinct() {
        for (source, want) in [
            ("UNION", SetOp::Union),
            ("EXCEPT", SetOp::Except),
            ("INTERSECT", SetOp::Intersect),
        ] {
            let q = parsed(&format!(
                "MATCH (a:Person) RETURN a AS x {source} MATCH (b:Person) RETURN b AS x"
            ));
            let Composite::Conjoined { how, .. } = &q.body else {
                panic!("{source} joins two operands");
            };
            assert_eq!(
                *how,
                Conjunction::Set {
                    op: want,
                    all: false
                }
            );
        }
    }

    /// OTHERWISE joins two result tables like the set operators do,
    /// but it is not one of them and carries no quantifier.
    #[test]
    fn otherwise_is_a_conjunction_of_its_own() {
        let q = parsed("MATCH (a:Person) RETURN a AS x OTHERWISE MATCH (b:Person) RETURN b AS x");
        let Composite::Conjoined { how, .. } = &q.body else {
            panic!("OTHERWISE joins two operands");
        };
        assert_eq!(*how, Conjunction::Otherwise);
    }

    /// A conjunction meets two result tables, so an operand that
    /// returned none of one is told what is missing rather than being
    /// told the word it ran into was unexpected.
    #[test]
    fn an_operand_of_a_conjunction_has_to_return() {
        let err = parse_err("MATCH (n:Person) UNION MATCH (m:Person) RETURN m");
        assert!(
            err.contains("UNION joins two result tables, so the statement in front of it has to end with RETURN"),
            "{err}"
        );
        let err = parse_err("INSERT (x:Person) OTHERWISE MATCH (m:Person) RETURN m");
        assert!(
            err.contains("OTHERWISE joins two result tables, so the statement in front of it has to end with RETURN"),
            "{err}"
        );
    }

    #[test]
    fn a_use_clause_says_which_graph_the_clauses_are_against() {
        let q = parsed("USE CURRENT_PROPERTY_GRAPH MATCH (n:Person) RETURN n");
        assert_eq!(q.use_graph, Some(GraphRef::Current));
        assert_eq!(
            q.clauses().len(),
            1,
            "the MATCH, the RETURN being the result"
        );

        let q = parsed("USE social MATCH (n) RETURN n");
        assert_eq!(
            q.use_graph,
            Some(GraphRef::Named(GraphName {
                schema: None,
                name: "social".to_string(),
            }))
        );

        // The long spelling and a path say the same as the short one.
        let q = parsed("USE PROPERTY GRAPH /app/social MATCH (n) RETURN n");
        assert_eq!(
            q.use_graph,
            Some(GraphRef::Named(GraphName {
                schema: Some("/app".to_string()),
                name: "social".to_string(),
            }))
        );

        assert_eq!(parsed("MATCH (n) RETURN n").use_graph, None);
    }

    /// The two graphs a session has words for, and the graph it is
    /// handed. The last is the graph reference form: the text says
    /// which parameter, and the parameter says which graph.
    #[test]
    fn a_use_clause_takes_a_graph_reference_and_not_only_a_name() {
        let q = parsed("USE HOME_PROPERTY_GRAPH MATCH (n) RETURN n");
        assert_eq!(q.use_graph, Some(GraphRef::Home));
        assert_eq!(
            parsed("USE HOME_GRAPH MATCH (n) RETURN n").use_graph,
            Some(GraphRef::Home),
            "the short spelling is the same graph"
        );

        let q = parsed("USE $g MATCH (n) RETURN n");
        assert_eq!(q.use_graph, Some(GraphRef::Param("g".to_string())));

        // A parameter stands where a name stands, so the word in front
        // of it reads the same way it does in front of a name.
        assert_eq!(
            parsed("USE GRAPH $g MATCH (n) RETURN n").use_graph,
            Some(GraphRef::Param("g".to_string()))
        );
        assert_eq!(
            parsed("USE PROPERTY GRAPH $g MATCH (n) RETURN n").use_graph,
            Some(GraphRef::Param("g".to_string()))
        );

        // The graph a copy starts from is written the same way, so it
        // takes the same references.
        assert_eq!(
            catalog_stmt("CREATE GRAPH g ANY AS COPY OF $source"),
            CatalogStmt::CreateGraph {
                name: GraphName {
                    schema: None,
                    name: "g".to_string(),
                },
                if_not_exists: false,
                or_replace: false,
                of: GraphTypeRef::Any,
                copy_of: Some(GraphRef::Param("source".to_string())),
            }
        );
    }

    #[test]
    fn a_use_clause_still_wants_a_query_after_it() {
        assert!(parse_err("USE social").contains("empty query"));
        assert!(parse_err("USE").contains("a graph name"));
    }

    #[test]
    fn an_insert_carries_the_patterns_it_was_written_with() {
        let q = parsed("INSERT (x:person {name: 'zoe'}), (y:person) RETURN x, y");
        let Clause::Insert { patterns } = &q.clauses()[0] else {
            panic!("INSERT");
        };
        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[0].start.var.as_deref(), Some("x"));
        assert_eq!(
            patterns[0].start.label,
            Some(LabelExpr::Label("person".to_string()))
        );
        assert_eq!(patterns[0].start.props.len(), 1);
        assert_eq!(patterns[1].start.var.as_deref(), Some("y"));
        assert!(patterns[1].steps.is_empty());
    }

    /// An edge parses here and is refused later, because what the
    /// parser sees is a path like any other and which parts of the
    /// write surface are in yet is not a question about syntax.
    #[test]
    fn an_insert_takes_an_edge_pattern() {
        let q = parsed("INSERT (a:person)-[k:knows]->(b:person)");
        let Clause::Insert { patterns } = &q.clauses()[0] else {
            panic!("INSERT");
        };
        assert_eq!(patterns[0].steps.len(), 1);
    }

    /// A statement that writes has said what it is for by the time it
    /// ends, so it need not project anything; a statement that only
    /// reads still has to.
    #[test]
    fn a_write_can_end_without_a_return_and_a_read_cannot() {
        let q = parsed("INSERT (x:person {name: 'zoe'})");
        assert_eq!(q.clauses().len(), 1);
        assert!(parse_err("MATCH (n:person)").contains("RETURN"));
        // A SET is a write too, so the same statement ends the same
        // way.
        assert_eq!(parsed("MATCH (p:person) SET p.age = 1").clauses().len(), 2);
    }

    #[test]
    fn a_set_carries_the_assignments_it_was_written_with() {
        let q = parsed("MATCH (p:person) SET p.age = 37, p.name = 'zoe'");
        let Clause::Set { items } = &q.clauses()[1] else {
            panic!("SET");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].target, "p");
        assert_eq!(items[0].into, SetInto::Property("age".into()));
        assert_eq!(items[1].into, SetInto::Property("name".into()));
    }

    /// The whole record form names no key and carries the fields as the
    /// record they were written as, so what tells the two forms apart
    /// downstream is the missing key and nothing else.
    #[test]
    fn a_set_of_a_whole_record_names_no_key_and_carries_the_fields() {
        let q = parsed("MATCH (p:person) SET p = {age: 37, name: 'zoe'}");
        let Clause::Set { items } = &q.clauses()[1] else {
            panic!("SET");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].target, "p");
        assert_eq!(items[0].into, SetInto::Record);
        let Expr::Map(fields) = &items[0].value else {
            panic!("a record");
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].0, "age");
        assert_eq!(fields[1].0, "name");
    }

    /// An empty record is the way to say that an element holds nothing,
    /// and it parses rather than being a special case.
    #[test]
    fn a_set_of_an_empty_record_parses() {
        let q = parsed("MATCH (p:person) SET p = {}");
        let Clause::Set { items } = &q.clauses()[1] else {
            panic!("SET");
        };
        assert_eq!(items[0].value, Expr::Map(Vec::new()));
    }

    /// The right hand side of the whole record form is a record written
    /// out, so anything else is a syntax error and says so.
    #[test]
    fn a_set_of_a_whole_record_wants_a_record() {
        let err = parse_err("MATCH (p:person) SET p = 37");
        assert!(err.contains('{'), "{err}");
    }

    #[test]
    fn a_remove_carries_the_properties_it_was_written_with() {
        let q = parsed("MATCH (p:person) REMOVE p.age, p.name");
        let Clause::Remove { items } = &q.clauses()[1] else {
            panic!("REMOVE");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].target, "p");
        assert_eq!(items[0].what, Removed::Property("age".into()));
        assert_eq!(items[1].what, Removed::Property("name".into()));
        // A REMOVE is a write, so the statement ends without a RETURN
        // the way a SET does.
        assert_eq!(q.clauses().len(), 2);
    }

    /// The names of the delete items that are variables, which is what
    /// most of these cases are about.
    fn vars(targets: &[DeleteTarget]) -> Vec<&str> {
        targets
            .iter()
            .map(|target| match target {
                DeleteTarget::Variable(name) => name.as_str(),
                DeleteTarget::Value(_) => panic!("a query, not a variable"),
            })
            .collect()
    }

    #[test]
    fn a_delete_carries_the_variables_it_was_written_with() {
        let q = parsed("MATCH (p:person), (q:person) DELETE p, q");
        let Clause::Delete { targets, detach } = &q.clauses()[1] else {
            panic!("DELETE");
        };
        assert_eq!(vars(targets), ["p", "q"]);
        assert!(!detach, "no DETACH was written");
        // A DELETE is a write, so the statement ends without a RETURN
        // the way a SET does.
        assert_eq!(q.clauses().len(), 2);
    }

    /// `DETACH` is the word that says the edges go too, and it reaches
    /// the clause rather than being another way of writing DELETE.
    #[test]
    fn a_detach_delete_says_so_on_the_clause() {
        let q = parsed("MATCH (p:person) DETACH DELETE p");
        let Clause::Delete { targets, detach } = &q.clauses()[1] else {
            panic!("DELETE");
        };
        assert_eq!(vars(targets), ["p"]);
        assert!(detach, "DETACH was written");
    }

    /// NODETACH says out loud what a plain DELETE says by saying
    /// nothing, so the clause it parses to is the same one.
    #[test]
    fn nodetach_delete_is_a_plain_delete() {
        let q = parsed("MATCH (p:person) NODETACH DELETE p");
        let Clause::Delete { targets, detach } = &q.clauses()[1] else {
            panic!("DELETE");
        };
        assert_eq!(vars(targets), ["p"]);
        assert!(!detach, "NODETACH is the default spelled out");
    }

    /// DELETE takes a variable and not an expression, so a property
    /// reference after it is a syntax error and says what it wanted.
    #[test]
    fn deleting_something_that_is_not_a_variable_is_refused() {
        assert!(
            parse_err("MATCH (p:person) DELETE p.age").contains("an element and not a property")
        );
        assert!(parse_err("MATCH (p:person) DELETE").contains("a variable after DELETE"));
    }

    /// GD03: a delete item can be a query, and the query comes back
    /// parsed rather than as the text between the braces.
    #[test]
    fn a_delete_item_can_be_a_value_query_expression() {
        let q = parsed("DELETE VALUE { MATCH (p:person) WHERE p.age > 30 RETURN p }");
        let Clause::Delete { targets, detach } = &q.clauses()[0] else {
            panic!("DELETE");
        };
        assert!(!detach, "no DETACH was written");
        let [DeleteTarget::Value(nested)] = &targets[..] else {
            panic!("one item, a query");
        };
        assert_eq!(nested.clauses().len(), 1, "the MATCH");
        assert!(nested.result().is_some(), "the RETURN");
    }

    /// The braces are matched over the tokens, so a query with braces
    /// of its own inside it ends where its own closing brace is and not
    /// at the first one.
    #[test]
    fn a_nested_query_ends_at_its_own_closing_brace() {
        let q = parsed(
            "MATCH (p:person) DELETE p, VALUE { MATCH (q:person {name: 'ada'}) RETURN q }, p",
        );
        let Clause::Delete { targets, .. } = &q.clauses()[1] else {
            panic!("DELETE");
        };
        assert_eq!(targets.len(), 3, "two variables around one query");
        assert!(matches!(targets[1], DeleteTarget::Value(_)));
    }

    /// A subquery is parsed where the statement holding it is, so a
    /// syntax error inside one is a syntax error and not something the
    /// first run finds.
    #[test]
    fn a_broken_query_inside_a_delete_item_is_a_syntax_error() {
        assert!(parse_err("DELETE VALUE { MATCH (p:person) RETRUN p }").contains("42001"));
        assert!(parse_err("DELETE VALUE { }").contains("the braces after it hold nothing"));
        assert!(parse_err("DELETE VALUE { MATCH (p:person) RETURN p").contains("never closed"));
    }

    /// VALUE is not reserved, so a variable somebody called that is
    /// still a variable: what makes the item a query is the brace.
    #[test]
    fn a_variable_called_value_is_read_as_a_variable() {
        let q = parsed("MATCH (value:person) DELETE value");
        let Clause::Delete { targets, .. } = &q.clauses()[1] else {
            panic!("DELETE");
        };
        assert_eq!(vars(targets), ["value"]);
    }

    /// A label is the other thing GQL lets REMOVE take, in either of
    /// the two spellings of the colon, and a set of them comes off in
    /// one item.
    #[test]
    fn a_remove_of_labels_carries_them_in_one_item() {
        for source in [
            "MATCH (p:person) REMOVE p:Manager&Bot",
            "MATCH (p:person) REMOVE p IS Manager&Bot",
        ] {
            let q = parsed(source);
            let Clause::Remove { items } = &q.clauses()[1] else {
                panic!("REMOVE");
            };
            assert_eq!(items.len(), 1, "{source}");
            assert_eq!(items[0].target, "p");
            assert_eq!(
                items[0].what,
                Removed::Labels(vec!["Manager".into(), "Bot".into()]),
                "{source}"
            );
        }
    }

    /// A SET of labels carries the labels and a null value, because what
    /// it writes is written in the statement rather than on the right of
    /// an equals sign.
    #[test]
    fn a_set_of_labels_carries_them_and_no_value() {
        for source in [
            "MATCH (p:person) SET p:Manager&Bot",
            "MATCH (p:person) SET p IS Manager&Bot",
        ] {
            let q = parsed(source);
            let Clause::Set { items } = &q.clauses()[1] else {
                panic!("SET");
            };
            assert_eq!(items.len(), 1, "{source}");
            assert_eq!(
                items[0].into,
                SetInto::Labels(vec!["Manager".into(), "Bot".into()]),
                "{source}"
            );
            assert_eq!(items[0].value, Expr::Literal(Literal::Null));
        }
    }

    /// A colon with no label after it is a syntax error that says what
    /// was wanted, rather than an item that writes nothing.
    #[test]
    fn a_label_item_wants_a_label() {
        assert!(parse_err("MATCH (p:person) SET p:").contains("a label"));
        assert!(parse_err("MATCH (p:person) REMOVE p:").contains("a label"));
    }

    /// Every syntax error that names a place carries that place as
    /// fields as well as saying it, so a caller can point at the token
    /// without reading the numbers back out of the sentence.
    #[test]
    fn a_syntax_error_carries_its_place_and_still_says_it() {
        let source = "MATCH (n) RETURN n RETURN n";
        let e = parse(source).expect_err("should fail");
        assert_eq!(
            e.position(),
            Some(Position {
                offset: 19,
                line: 1,
                column: 20
            })
        );
        assert!(
            e.to_string().starts_with("42001: line 1, column 20: "),
            "{e}"
        );
        // The offset is the same place counted the way a program reads
        // it, so the token the message is about can be sliced out.
        assert_eq!(&source[19..], "RETURN n");

        // A second line is counted as a second line, and the column
        // starts over on it while the offset keeps counting.
        let e = parse("MATCH (n)\n  RETURN n RETURN n").expect_err("should fail");
        assert_eq!(
            e.position(),
            Some(Position {
                offset: 21,
                line: 2,
                column: 12
            })
        );
        // And the line it is on comes back with it, so whatever prints
        // this does not need the query to show where it went wrong.
        assert_eq!(e.excerpt(), Some("  RETURN n RETURN n"));

        // The lexer's own failures are positioned the same way, since
        // a caller cannot tell which half of the front end refused it.
        let e = parse("RETURN 'unterminated").expect_err("should fail");
        assert_eq!(
            e.position(),
            Some(Position {
                offset: 7,
                line: 1,
                column: 8
            })
        );
        assert_eq!(e.excerpt(), Some("RETURN 'unterminated"));

        // Running out of text is not a place any token is at, so that
        // one says so in words and carries no place and no line.
        let e = parse("MATCH (n) WHERE").expect_err("should fail");
        assert_eq!(e.position(), None);
        assert_eq!(e.excerpt(), None);
        assert!(e.to_string().contains("unexpected end of query"), "{e}");
    }

    #[test]
    fn keywords_are_case_insensitive() {
        let q = parsed("match (n:Person) where n.id = 1 return n limit 1");
        assert_eq!(
            q.clauses().len(),
            1,
            "the MATCH, the RETURN being the result"
        );
        let projection = q.result().expect("RETURN");
        assert!(projection.limit.is_some());
    }
}
