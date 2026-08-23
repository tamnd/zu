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
use zu_common::keywords;
use zu_common::unicode::NormalForm;
use zu_common::{
    DurationKind, Field, IntervalField, IntervalQualifier, LogicalType, RecordType, Result,
    Temporal, ZuError,
};

use crate::ast::{
    BinaryOp, BindingDef, BindingInit, BindingKind, Brackets, CatalogStmt, Clause, Composite,
    Conjunction, DeleteTarget, EdgeEnd, ElementDefKind, ElementTypeDef, Endpoint, Expr, GraphName,
    GraphRef, GraphTypeRef, GraphTypeSource, Group, GroupKind, LabelExpr, LetItem, Linear, Literal,
    MatchMode, NodePattern, NullOrder, Ordinal, PathMode, PathPattern, PatternList, ProcRef,
    Projection, ProjectionItem, PropertyDef, Query, RelDirection, RelPattern, RemoveItem, Removed,
    Repeat, SchemaRef, Selector, SessionReset, SessionStmt, SetInto, SetItem, SetOp, Simple,
    SortKey, Statement, Subpath, TemporalFn, TrimSide, TruthValue, TxnStmt, UnaryOp, YieldItem,
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
/// The ways a graph pattern was written: the first list, the lists
/// behind it, and which bar separated them.
///
/// The bar is `None` for a pattern that wrote none, which is every
/// pattern of one way and every pattern whose ways are the lengths of a
/// quantified stretch rather than alternatives someone wrote a bar
/// between. What it decides is duplicates, and there are none to decide
/// about where no bar was written.
type Ways = (Vec<PathPattern>, Vec<Vec<PathPattern>>, Option<bool>);

#[derive(Default, Clone)]
struct Segment {
    nodes: Vec<NodePattern>,
    rels: Vec<RelPattern>,
    subpaths: Vec<Subpath>,
    groups: Vec<Group>,
    repeats: Vec<Repeat>,
    filter: Option<Expr>,
}

/// One step of what a simplified path pattern describes (ISO 16.12),
/// before the arrow around it has said which way the steps that wrote
/// no direction of their own go.
#[derive(Clone)]
struct Simplified {
    /// The types the step may walk, which is what the labels the
    /// expression wrote for this step come to.
    types: Vec<String>,
    /// The direction this step overrode the pattern's with, if it wrote
    /// one (features G081 and G082).
    direction: Option<RelDirection>,
    /// The hops the step walks, when a quantifier was written on it and
    /// there was one step for it to be about.
    range: Option<(Option<u64>, Option<u64>)>,
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

/// The reason a pattern written several ways is refused where one
/// shape belongs, which is an `INSERT` or a `MERGE`.
///
/// Both spellings say the same thing and they are told apart because a
/// reader is owed the reason they wrote rather than the reason the
/// parser reached: a bar is a pattern written two ways on purpose, and
/// a quantifier of several lengths is one the writer may not have
/// noticed is several patterns.
fn one_shape(bar: bool) -> &'static str {
    if bar {
        "a bar between two path patterns says either of them may be matched, \
         and this is a position that describes one shape rather than looking \
         for one"
    } else {
        "a stretch repeated a variable number of times matches paths of several \
         lengths, which is several shapes, and this is a position that describes \
         one shape rather than looking for one; write a fixed count"
    }
}

/// Which edges an arrow walks, read off the three marks it may carry:
/// the `<` in front of it, the tilde that stands where a dash would,
/// and the `>` behind it. `None` is the one combination that says two
/// things at once, an edge with no direction that points both ways.
///
/// The seven that are left are the seven edge patterns of ISO 18.9,
/// and they are also the seven ways a simplified path pattern spells
/// which way its edges go, so both are read here.
fn a_direction(inbound: bool, tilde: bool, outbound: bool) -> Option<RelDirection> {
    match (inbound, tilde, outbound) {
        (true, true, true) => None,
        (true, false, false) => Some(RelDirection::In),
        (false, false, true) => Some(RelDirection::Out),
        (true, false, true) => Some(RelDirection::AnyDirected),
        (false, true, false) => Some(RelDirection::Undirected),
        (true, true, false) => Some(RelDirection::InOrUndirected),
        (false, true, true) => Some(RelDirection::OutOrUndirected),
        (false, false, false) => Some(RelDirection::Any),
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
const UNIMPLEMENTED: &[&str] = &["CREATE"];

/// The column a select statement's having clause is carried in.
///
/// A having clause tests a value only the projection can work out, and
/// it runs between that projection and the order clause behind it, so
/// the value travels from the one to the other as a column. The name
/// begins with a hash because no identifier does, which is the same
/// thing the binder's own anonymous slots rely on, so no query can
/// name this column and none can shadow it.
const HAVING_COLUMN: &str = "#having";

/// Every word that can stand where a query begins once the schema
/// clause is read: the graph clause, the three that open a binding
/// variable definition, and the clauses themselves.
///
/// One reader, [`Parser::at_path_segment`], and one job: telling the
/// end of an `AT` path from the query behind it. The list being a
/// little long is the price of the lexer knowing nothing about
/// keywords, and a word missing from it costs a refusal rather than a
/// wrong answer, since the path would then swallow the word and no
/// schema of that name exists.
const OPENS_A_CLAUSE: &[&str] = &[
    "USE", "VALUE", "TABLE", "BINDING", "GRAPH", "PROPERTY", "MATCH", "OPTIONAL", "CALL", "INSERT",
    "MERGE", "SET", "REMOVE", "DELETE", "DETACH", "NODETACH", "UNWIND", "FOR", "FILTER", "LET",
    "WITH", "ORDER", "OFFSET", "SKIP", "LIMIT", "FINISH", "RETURN", "SELECT", "NEXT",
];

fn opens_a_clause(word: &str) -> bool {
    OPENS_A_CLAUSE
        .iter()
        .any(|kw| word.eq_ignore_ascii_case(kw))
}

/// How a simple query statement ended, which is what the parser needs
/// to say when something follows that may not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// A result statement, which is `RETURN` today.
    Result,
    /// A write that projected nothing.
    Write,
    /// `FINISH`, which is a result statement that says there is no
    /// result. Nothing may follow it and nothing may read from it,
    /// which is what separates it from the write above: a write that
    /// projected nothing ended because it had said everything it had
    /// to say, and this ended because the reader said so.
    Finish,
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
        Statement::Session(_) => Err(ZuError::gql(
            codes::C42001,
            "a session statement changes the session and answers no rows, so it runs through the session rather than the query path".to_string(),
        )),
        Statement::Block(_) => Err(ZuError::gql(
            codes::C42001,
            "a statement block holds a catalog statement among its parts, so it runs through the session rather than the query path".to_string(),
        )),
    }
}

/// Parses one statement: a query, a catalog statement, a transaction
/// statement, a session statement, or a block of them chained by
/// `NEXT`. The first word tells the first four apart, and a `NEXT`
/// handing over to a catalog statement is what makes it the fifth.
pub fn parse_statement(source: &str) -> Result<Statement> {
    let tokens = lex(source)?;
    let mut parser = Parser {
        source,
        tokens,
        pos: 0,
        depth: 0,
        lists: 0,
        ends_a_definition: false,
        lengths: Vec::new(),
        taken: 0,
        spans: Vec::new(),
        lifted: Vec::new(),
        hoisted: None,
        hoisted_at: None,
        hoisted_use: None,
        hoisted_bindings: Vec::new(),
        select_body: false,
        select_graph: None,
    };
    if parser.at_txn_stmt() {
        let stmt = parser.parse_txn_stmt()?;
        return Ok(Statement::Transaction(stmt));
    }
    if parser.at_kw("SESSION") {
        let stmt = parser.parse_session_stmt()?;
        return Ok(Statement::Session(stmt));
    }
    // GP18, ISO 13.6. A linear statement is all query or all catalog,
    // so a chain that is neither is a statement block, and this is the
    // one place it can be told: the parts have been read and one of
    // them changes the catalog.
    let start = parser.here();
    let (first, mut ending) = parser.parse_block_part()?;
    let mut spans = vec![(start, parser.here())];
    while parser.eat_kw("NEXT") {
        let at = parser.here();
        let (_, next) = parser.parse_block_part()?;
        ending = next;
        spans.push((at, parser.here()));
    }
    parser.finish(ending)?;
    if spans.len() == 1 && parser.lifted.is_empty() {
        return Ok(first);
    }
    Ok(Statement::Block(parser.block_parts(&spans)))
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
    /// Whether an `IN` ahead closes the definitions of a `LET`
    /// expression rather than testing membership, GE03.
    ///
    /// The two readings collide by the standard's own grammar, since a
    /// definition holds a whole value expression and `IN` is one of the
    /// operators a value expression is made of, so `LET n = a IN b IN c
    /// END` has two parses and neither is more honest than the other.
    /// This is the resolution: at the top of a definition the word ends
    /// the definitions, and a membership test written there goes in
    /// parentheses. It is set for the top of a definition alone, so
    /// `LET n = (a IN b) IN n END` reads the test, and everywhere a
    /// nested expression begins it is off again.
    ends_a_definition: bool,
    /// The length chosen for each stretch of the path term being read
    /// that repeats a variable number of times, in the order the
    /// stretches are written.
    ///
    /// A term holding such a stretch is read once per length, because a
    /// stretch of several lengths is several patterns and not one, so
    /// the term is parsed as many times as there are lengths to choose
    /// between and this says which choice the pass in hand is making.
    /// Empty for the pass that discovers what the choices are, where
    /// every stretch takes its least length.
    lengths: Vec<usize>,
    /// How many of those stretches this pass has read, which indexes
    /// the choice the next one takes.
    taken: usize,
    /// The range each of those stretches was written with, gathered as
    /// they are read, which is what the choices are made out of.
    spans: Vec<(usize, usize)>,
    /// The catalog statements taken out of a call body at the head of
    /// the statement and put in front of it (GP18).
    lifted: Vec<Lift>,
    /// The statements of the body of a call the statement begins with,
    /// waiting for the chain they go in, and how the last of them
    /// ended. See `hoist_a_leading_call`.
    hoisted: Option<(Vec<Simple>, Ending)>,
    /// The schema clause that body was written with, which belongs to
    /// the query the statements go into.
    hoisted_at: Option<SchemaRef>,
    /// The graph clause that body was written with, and how many
    /// statements came out of the body with it.
    ///
    /// The count is what says which statements of the chain the clause
    /// was written around: zu runs one graph per query, so a graph
    /// clause taken out of a body governs everything in the chain, and
    /// the statements behind the body were written outside it. Those
    /// may only project what the body answered, and the count is how
    /// [`Parser::parse_query_body`] finds them to say so.
    hoisted_use: Option<(GraphRef, usize)>,
    /// The definitions that body was written with, which come out with
    /// the graph clause because the clause may read one of them.
    hoisted_bindings: Vec<BindingDef>,
    /// Whether a select statement body is being read, which is the one
    /// place a comma may end a match statement rather than joining
    /// another pattern to its list. See
    /// [`Parser::comma_ends_a_select_graph_match`].
    select_body: bool,
    /// The graph a select statement body named, waiting for the query
    /// it belongs to.
    ///
    /// ISO 14.12 writes the graph expression inside the body, once per
    /// match, where every other statement here writes it as a `USE`
    /// clause in front. zu runs one graph per query, so what the body
    /// named is the query's, and it is carried out here rather than
    /// turned into a clause because a clause is a thing the reader
    /// wrote and this is a thing the reader wrote somewhere else.
    select_graph: Option<GraphRef>,
}

/// A catalog statement lifted out of a call body (GP18).
///
/// `from` and `to` are the statement itself, which becomes a part of
/// the block, and `from` to `cut` is what comes out of the text the
/// rest of the statement is made of: the statement and the `NEXT`
/// handing over to what follows it.
struct Lift {
    from: usize,
    to: usize,
    cut: usize,
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
    /// An `<identifier>` (ISO 21.3): a regular one, or a delimited one
    /// written either of the two ways the standard writes it.
    ///
    /// A reserved word is not a regular identifier, and the rule is in
    /// the Syntax Rules rather than in the productions, so it is
    /// enforced here. Delimiting the word puts it back:
    /// `RETURN 1 AS MATCH` is refused and ``RETURN 1 AS `MATCH` `` is
    /// not, which is what the delimited form is for.
    fn expect_name(&mut self, what: &str) -> Result<String> {
        self.name(what, false)
    }

    /// A property name or a record field name, which admits a reserved
    /// word where the other name slots do not.
    ///
    /// Read strictly, ISO refuses one here too: `<property name>` and
    /// `<field name>` are both `<identifier>` and the 21.3 rule reaches
    /// every one of them. But the standard writes its own datetime
    /// constructors as records with the fields `year`, `month`, `day`,
    /// `hour`, `minute` and `second`, and all six of those words are in
    /// `<reserved word>`, so `DATE({year: 2024, month: 1, day: 1})` is
    /// a statement the standard both defines and forbids. Something has
    /// to give, and the rule is what gives: a name in this slot always
    /// stands between a delimiter and a colon or behind a dot, so no
    /// word here could be read as the keyword it is spelled like, and
    /// the rule buys nothing where there is nothing to disambiguate.
    ///
    /// The slots where a word could be read either way keep the rule.
    /// `RETURN 1 AS year` is still refused, and so is `MATCH (year)`.
    /// This is a deviation and `docs/07-query-engine.md` records it.
    fn expect_field_name(&mut self, what: &str) -> Result<String> {
        self.name(what, true)
    }

    fn name(&mut self, what: &str, reserved_admitted: bool) -> Result<String> {
        let Some(token) = self.peek() else {
            return Err(self.error(what));
        };
        let name = match &token.kind {
            TokenKind::Ident(s) => {
                if !reserved_admitted && keywords::is_reserved(s) {
                    let at = token.start;
                    let word = s.clone();
                    return Err(ZuError::gql_in(
                        codes::C42001,
                        self.source,
                        at,
                        format_args!(
                            "'{word}' is a reserved word and a name written plainly is not one; \
                             write it in accent quotes to use it as {what}"
                        ),
                    ));
                }
                s.clone()
            }
            TokenKind::QuotedIdent(s) => s.clone(),
            // A double quoted sequence is a delimited identifier and a
            // character string literal both, and which one it is comes
            // from where it stands. Here a name is what is wanted, so
            // it is a name; a single quoted one is only ever a string
            // and stays out.
            TokenKind::Str(s) if self.double_quoted(token) => s.clone(),
            _ => return Err(self.error(what)),
        };
        self.pos += 1;
        Ok(name)
    }

    /// Whether the token was written with double quotes, which is what
    /// tells a delimited identifier from a string literal. The lexer
    /// makes one token of both, because they hold the same characters
    /// and differ only in where they may stand.
    fn double_quoted(&self, token: &Token) -> bool {
        let at = self.source[token.start..].trim_start_matches('@');
        at.starts_with('"')
    }

    /// A `<binding variable>` (ISO 16.4), which is a regular identifier
    /// and nothing else.
    ///
    /// The chain is `<element variable>` to `<binding variable>` to
    /// `<regular identifier>`, one alternative at every step, so a
    /// delimited identifier does not reach this slot however it is
    /// written. Cypher admits a backticked variable and engines grown
    /// out of Cypher take one; this one says what the grammar says.
    fn expect_variable(&mut self, what: &str) -> Result<String> {
        if let Some(token) = self.peek()
            && matches!(token.kind, TokenKind::QuotedIdent(_) | TokenKind::Str(_))
            && !matches!(token.kind, TokenKind::Str(_) if !self.double_quoted(token))
        {
            let at = token.start;
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                at,
                format_args!(
                    "a variable is a plain name, and a delimited identifier is not one; \
                     {what} has to be written without quotes"
                ),
            ));
        }
        self.expect_name(what)
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

    /// `SESSION SET ...`, `SESSION RESET ...` and `SESSION CLOSE` (ISO
    /// 7.1 through 7.3, GS01 through GS16).
    ///
    /// The whole family is one word followed by one of three verbs, so
    /// there is nothing else `SESSION` can open and no lookahead is
    /// needed to know one is here. What comes after the verb is a small
    /// matrix rather than fifteen statements: three parameter kinds
    /// over two value sources, and the schema, the graph and the zone
    /// beside them. The close command is the one with nothing under it
    /// at all.
    fn parse_session_stmt(&mut self) -> Result<SessionStmt> {
        self.expect_kw("SESSION")?;
        let stmt = if self.eat_kw("RESET") {
            SessionStmt::Reset(self.parse_session_reset()?)
        } else if self.eat_kw("CLOSE") {
            SessionStmt::Close
        } else if self.eat_kw("SET") {
            self.parse_session_set()?
        } else {
            return Err(self.error("SET, RESET or CLOSE after SESSION"));
        };
        self.eat(&TokenKind::Semicolon);
        if let Some(token) = self.peek() {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                token.start,
                format_args!(
                    "nothing may follow a session statement, found {}",
                    token.kind.describe()
                ),
            ));
        }
        Ok(stmt)
    }

    /// What a `SESSION SET` sets.
    ///
    /// The three parameter clauses and the two clauses that move the
    /// session itself are told apart by the word after the verb, and
    /// only the graph is written both ways: `SESSION SET PROPERTY GRAPH
    /// $p = g` binds a parameter and `SESSION SET PROPERTY GRAPH g`
    /// moves the session's graph, so the dollar is what decides.
    fn parse_session_set(&mut self) -> Result<SessionStmt> {
        if self.at_kw("TIME") && self.kw_at(1, "ZONE") {
            self.pos += 2;
            return Ok(SessionStmt::SetTimeZone(self.parse_time_zone()?));
        }
        if self.at_kw("SCHEMA") {
            self.pos += 1;
            // The same three a schema is named by anywhere else: the
            // two words and a path. `CURRENT_SCHEMA` moves a session
            // nowhere and `HOME_SCHEMA` moves it back, which is the
            // pair a reader needs to write the round trip without
            // knowing where the session opened.
            if self.eat_kw("CURRENT_SCHEMA") {
                return Ok(SessionStmt::SetSchema(SchemaRef::Current));
            }
            if self.eat_kw("HOME_SCHEMA") {
                return Ok(SessionStmt::SetSchema(SchemaRef::Home));
            }
            // The third predefined reference is a period on its own,
            // which names the schema the session is in already and so
            // says what `CURRENT_SCHEMA` says in one character.
            if self.eat(&TokenKind::Dot) {
                return Ok(SessionStmt::SetSchema(SchemaRef::Current));
            }
            if self.at(&TokenKind::DotDot) {
                return Ok(SessionStmt::SetSchema(self.parse_relative_schema(false)?));
            }
            return Ok(SessionStmt::SetSchema(SchemaRef::Path(
                self.parse_schema_path()?,
            )));
        }
        let kind = if self.at_kw("VALUE") {
            self.pos += 1;
            BindingKind::Value
        } else if self.at_kw("TABLE") || (self.at_kw("BINDING") && self.kw_at(1, "TABLE")) {
            self.eat_kw("BINDING");
            self.pos += 1;
            BindingKind::Table
        } else if self.at_kw("GRAPH") || (self.at_kw("PROPERTY") && self.kw_at(1, "GRAPH")) {
            let at = usize::from(self.at_kw("PROPERTY"));
            let parameter = matches!(
                self.tokens.get(self.pos + at + 1).map(|t| &t.kind),
                Some(TokenKind::Param(_))
            );
            self.eat_kw("PROPERTY");
            self.pos += 1;
            // ISO writes the graph the session works in as a graph
            // expression behind the word, so the word is eaten here and
            // what follows is read the way a `USE` reads it: the four
            // that name a graph, one of them being a name.
            if !parameter {
                return Ok(SessionStmt::SetGraph(self.parse_graph_ref()?));
            }
            BindingKind::Graph
        } else {
            return Err(self.error(
                "VALUE, BINDING TABLE, PROPERTY GRAPH, SCHEMA or TIME ZONE after SESSION SET",
            ));
        };
        // ISO writes the modifier in front of the name rather than
        // behind it, `SESSION SET VALUE IF NOT EXISTS $p = 1`, which is
        // the other way round from the catalog statements.
        let if_not_exists = self.eat_if_exists(true)?;
        let def = self.parse_session_param_def(kind)?;
        Ok(SessionStmt::SetParameter { def, if_not_exists })
    }

    /// One parameter definition, which is a binding variable definition
    /// whose name is written with a dollar (ISO 7.1).
    ///
    /// The two are the same rule with two spellings of the name, so the
    /// name is read here and the rest is [`Self::parse_binding_def`]'s.
    /// That is not a shortcut: a session parameter and a binding
    /// variable stand for the same three kinds of thing, take the same
    /// optional type, and are initialized from the same expression or
    /// the same query in braces, so two readers would be two chances to
    /// accept different languages under one grammar.
    fn parse_session_param_def(&mut self, kind: BindingKind) -> Result<BindingDef> {
        let Some(TokenKind::Param(name)) = self.peek().map(|t| t.kind.clone()) else {
            return Err(self.error("a session parameter name, written with a dollar"));
        };
        self.pos += 1;
        self.parse_binding_def_body(kind, name)
    }

    /// The displacement a `SESSION SET TIME ZONE` names (GS15).
    ///
    /// ISO writes it as a character string, and what zu takes is a
    /// displacement and never a zone name (`02 §3.4`): a name is a rule
    /// the zone database can change, so a session set to one would mean
    /// a different instant after an upgrade. The string is read here
    /// rather than at the session, because a string that is not a
    /// displacement is a fault in the statement.
    fn parse_time_zone(&mut self) -> Result<i16> {
        let Some(Token {
            kind: TokenKind::Str(text),
            start,
            ..
        }) = self.peek().cloned()
        else {
            return Err(self.error("a time zone displacement in quotes, such as '+07:00'"));
        };
        self.pos += 1;
        zu_common::temporal::zone_offset(&text).ok_or_else(|| {
            ZuError::gql_in(
                codes::C22007,
                self.source,
                start,
                format_args!(
                    "'{text}' is no time zone displacement: a session takes an offset from UTC, written 'Z' or '+hh', '+hhmm' or '+hh:mm' either way of nought, and never a zone name, since a name is a rule the zone database can change under a session that is holding it"
                ),
            )
        })
    }

    /// What a `SESSION RESET` puts back (ISO 7.2).
    ///
    /// `SESSION RESET` on its own resets everything, which is what the
    /// standard says a reset with no arguments means and is the same
    /// thing `ALL CHARACTERISTICS` spells out.
    fn parse_session_reset(&mut self) -> Result<SessionReset> {
        let all = self.eat_kw("ALL");
        if self.eat_kw("CHARACTERISTICS") {
            return Ok(SessionReset::Characteristics);
        }
        if self.eat_kw("PARAMETERS") {
            return Ok(SessionReset::Parameters);
        }
        if all {
            return Err(self.error("PARAMETERS or CHARACTERISTICS after SESSION RESET ALL"));
        }
        if self.eat_kw("SCHEMA") {
            return Ok(SessionReset::Schema);
        }
        if self.at_kw("TIME") && self.kw_at(1, "ZONE") {
            self.pos += 2;
            return Ok(SessionReset::TimeZone);
        }
        if self.at_kw("GRAPH") || (self.at_kw("PROPERTY") && self.kw_at(1, "GRAPH")) {
            self.eat_kw("PROPERTY");
            self.pos += 1;
            return Ok(SessionReset::Graph);
        }
        if self.eat_kw("PARAMETER") {
            let Some(TokenKind::Param(name)) = self.peek().map(|t| t.kind.clone()) else {
                return Err(self.error("a session parameter name, written with a dollar"));
            };
            self.pos += 1;
            return Ok(SessionReset::Parameter(name));
        }
        // A reset with nothing after it is the widest one, so the end
        // of the statement is an answer and anything else is not.
        if self.peek().is_none() || self.at(&TokenKind::Semicolon) {
            return Ok(SessionReset::Characteristics);
        }
        Err(self.error(
            "ALL CHARACTERISTICS, ALL PARAMETERS, PARAMETER $p, SCHEMA, PROPERTY GRAPH, TIME ZONE, or nothing at all after SESSION RESET",
        ))
    }

    /// Whether this statement changes the catalog rather than reading
    /// the graph. `CREATE` opens statements that are not here yet, so
    /// the words after it decide: `GRAPH`, `SCHEMA`, and `PROPERTY
    /// GRAPH` and `OR REPLACE` ahead of either, are all catalog
    /// statements.
    fn at_catalog_stmt(&self) -> bool {
        self.catalog_stmt_at(0)
    }

    /// The same test, `offset` tokens further on, which is what the
    /// word after a `NEXT` is asked (GP18).
    fn catalog_stmt_at(&self, offset: usize) -> bool {
        if !self.kw_at(offset, "CREATE") && !self.kw_at(offset, "DROP") {
            return false;
        }
        let mut at = offset + 1;
        if self.kw_at(at, "OR") && self.kw_at(at + 1, "REPLACE") {
            at += 2;
        }
        if self.kw_at(at, "PROPERTY") {
            at += 1;
        }
        self.kw_at(at, "GRAPH") || self.kw_at(at, "SCHEMA")
    }

    /// Whether the `NEXT` standing here hands over to a catalog
    /// statement, which is where a linear query statement ends and a
    /// statement block carries on (GP18). A linear query statement is
    /// query statements and nothing else, so the chain stops here and
    /// the entry point picks the next part up.
    fn next_hands_to_catalog(&self) -> bool {
        self.at_kw("NEXT") && self.catalog_stmt_at(1)
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
        // Three things may follow and all of them belong to the
        // caller: the semicolon that ends the text, a `NEXT` handing
        // over to the rest of a statement block (GP18), and the brace
        // closing a call body, which the call names better than this
        // could. None is eaten here, so the block reads a part that
        // stops where the part stops; anything else is a mistake named
        // here rather than left to a caller that would have to guess
        // what was written.
        if !self.at_kw("NEXT") && !self.at(&TokenKind::RBrace) {
            let past = usize::from(self.at(&TokenKind::Semicolon));
            if let Some(token) = self.tokens.get(self.pos + past) {
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
        let (schema, name) = self.parse_object_name("a graph name")?;
        Ok(GraphName { schema, name })
    }

    /// The schema an `AT` clause names and the procedure name after it.
    ///
    /// Whitespace is nothing to the lexer, so `AT / pagerank` and
    /// `AT /pagerank` are the same tokens and the path swallows the
    /// name. What tells them apart is what comes next: a call's name is
    /// followed by its arguments, so a path with nothing but a
    /// parenthesis behind it was a schema and a name written together,
    /// and it is split back into the two. A call that did write both,
    /// `AT /algo pagerank(...)`, never reaches the split, because the
    /// path stops where the name begins.
    fn parse_at_and_name(&mut self) -> Result<(Option<String>, ProcRef)> {
        let path = self.parse_schema_path()?;
        if !self.at(&TokenKind::LParen) {
            return Ok((Some(path), self.parse_proc_ref()?));
        }
        let cut = path.rfind('/').expect("a path begins with a slash");
        let name = path[cut + 1..].to_string();
        if name.is_empty() {
            return Err(self.error("a procedure name after AT and the schema"));
        }
        let schema = if cut == 0 {
            "/".to_string()
        } else {
            path[..cut].to_string()
        };
        Ok((Some(schema), ProcRef { schema: None, name }))
    }

    /// A procedure's name, which is a catalog object reference and so is
    /// read exactly the way a graph's name is.
    fn parse_proc_ref(&mut self) -> Result<ProcRef> {
        let (schema, name) = self.parse_object_name("a procedure name after CALL")?;
        Ok(ProcRef { schema, name })
    }

    /// A catalog object's name, either bare or written out as a path.
    /// The schema is what the path says without its last segment, and a
    /// name with one segment is in the root.
    fn parse_object_name(&mut self, what: &str) -> Result<(Option<String>, String)> {
        if !self.at(&TokenKind::Slash) {
            return Ok((None, self.expect_name(what)?));
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
        Ok((Some(schema), name))
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

    /// A relative catalog schema reference, ISO 4.3.2: a double period
    /// for each directory to climb and the names to walk down into
    /// after them, `../sibling` being the pair a schema writes to name
    /// the one beside it without knowing where either of them sits.
    ///
    /// `heading` says whether the reference is being read at the head
    /// of a query, where a segment that opens a clause was never a
    /// segment and the path has to end in front of it. A session
    /// statement has nothing behind the path, so it reads every name
    /// it finds.
    fn parse_relative_schema(&mut self, heading: bool) -> Result<SchemaRef> {
        let mut up = 0;
        let mut down: Vec<String> = Vec::new();
        loop {
            if !self.eat(&TokenKind::DotDot) {
                return Err(self.error("a double period"));
            }
            up += 1;
            if !self.eat(&TokenKind::Slash) {
                return Ok(SchemaRef::Relative { up, down });
            }
            if !self.at(&TokenKind::DotDot) {
                break;
            }
        }
        // The double periods are all in front, the grammar giving a
        // relative directory path no way to climb again once it has
        // walked down, so what is left is the plain names.
        while match heading {
            true => self.at_path_segment(0),
            false => self.at_name(),
        } {
            down.push(self.expect_name("a name in a path")?);
            if !self.eat(&TokenKind::Slash) {
                break;
            }
        }
        Ok(SchemaRef::Relative { up, down })
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
                let name = self.expect_field_name("a property name")?;
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
    /// The closing brace is here because a statement may be written
    /// inside a block, and the brace that ends the block ends the
    /// statement in it as surely as the end of the text would.
    fn at_statement_end(&self) -> bool {
        self.peek().is_none()
            || self.at(&TokenKind::Semicolon)
            || self.at(&TokenKind::RBrace)
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
                    | Clause::Merge { .. }
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
        let target = self.expect_variable("a variable after REMOVE")?;
        if self.eat(&TokenKind::Colon) || self.eat_kw("IS") {
            let labels = self.parse_label_set()?;
            return Ok(RemoveItem {
                target,
                what: Removed::Labels(labels),
            });
        }
        self.expect(&TokenKind::Dot)?;
        let key = self.expect_field_name("a property name after the dot")?;
        Ok(RemoveItem {
            target,
            what: Removed::Property(key),
        })
    }

    /// `MERGE (p:person {id: 7}) ON CREATE SET p.seen = 1 ON MATCH SET
    /// p.seen = p.seen + 1`, read after the word itself.
    ///
    /// One pattern and no comma. A comma here would be two patterns,
    /// and a statement that found the first and wrote the second is not
    /// a statement anyone means; Cypher spells that as two `MERGE`s and
    /// so does this.
    ///
    /// The two `ON` blocks may be written in either order and either
    /// may be left out. Writing one of them twice is refused rather
    /// than folded together, because the two spellings would run their
    /// items in different orders and the reader who wrote it that way
    /// meant one of them.
    fn parse_merge(&mut self) -> Result<Clause> {
        let pattern = self.parse_path()?;
        if self.at(&TokenKind::Comma) {
            return Err(self.error(
                "MERGE takes one pattern: it finds what it describes or writes it, and two \
                 patterns would leave which of the two it did unsaid",
            ));
        }
        let mut on_create = Vec::new();
        let mut on_match = Vec::new();
        let mut seen_create = false;
        let mut seen_match = false;
        while self.at_kw("ON") {
            let at = self.pos;
            self.eat_kw("ON");
            let create = if self.eat_kw("CREATE") {
                true
            } else if self.eat_kw("MATCH") {
                false
            } else {
                self.pos = at;
                break;
            };
            self.expect_kw("SET")?;
            let mut items = vec![self.parse_set_item()?];
            while self.eat(&TokenKind::Comma) {
                items.push(self.parse_set_item()?);
            }
            let (seen, into) = match create {
                true => (&mut seen_create, &mut on_create),
                false => (&mut seen_match, &mut on_match),
            };
            if *seen {
                return Err(self.error(match create {
                    true => "ON CREATE SET is written once: two of them would run in an order the statement does not say",
                    false => "ON MATCH SET is written once: two of them would run in an order the statement does not say",
                }));
            }
            *seen = true;
            *into = items;
        }
        Ok(Clause::Merge {
            pattern,
            on_create,
            on_match,
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
        let target = self.expect_variable("a variable after SET")?;
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
        let key = self.expect_field_name("a property name after the dot")?;
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
        let name = self.expect_variable("a variable name for the counter")?;
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
        let name = self.expect_variable("a variable name after LET")?;
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

    /// Where the parser stands, as a byte offset into the source, which
    /// is the end of the text when it has read all of it. It is what
    /// cuts one part of a statement block out of the whole (GP18).
    fn here(&self) -> usize {
        self.peek().map_or(self.source.len(), |t| t.start)
    }

    /// The text of each part of a statement block, in the order the
    /// parts run (GP18).
    ///
    /// The spans are the parts as they were written, one per link of
    /// the `NEXT` chain. Anything lifted out of a call body comes
    /// first, since a lift only happens at the head of the statement,
    /// and the text it was cut from is what is left of the span it fell
    /// in. Cuts do not overlap and are gathered in the order they were
    /// read, so one walk over each span is enough.
    fn block_parts(&self, spans: &[(usize, usize)]) -> Vec<String> {
        let mut parts: Vec<String> = self
            .lifted
            .iter()
            .map(|lift| self.source[lift.from..lift.to].trim().to_string())
            .collect();
        for &(from, to) in spans {
            let mut text = String::new();
            let mut at = from;
            for lift in &self.lifted {
                if lift.from < at || lift.cut > to {
                    continue;
                }
                text.push_str(&self.source[at..lift.from]);
                at = lift.cut;
            }
            text.push_str(&self.source[at..to]);
            parts.push(text.trim().to_string());
        }
        parts
    }

    /// The end of the text, which is where a statement ends: the
    /// optional semicolon after it, and nothing else past that.
    fn finish(&mut self, ending: Ending) -> Result<()> {
        self.eat(&TokenKind::Semicolon);
        if let Some(token) = self.peek() {
            let what = match ending {
                Ending::Result => "RETURN",
                Ending::Write => "the end of a statement",
                Ending::Finish => "FINISH",
            };
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                token.start,
                format_args!("nothing may follow {what}, found {}", token.kind.describe()),
            ));
        }
        Ok(())
    }

    /// One part of a statement block: a catalog statement or a query,
    /// and how it ended.
    ///
    /// A transaction statement is not one of them. A block runs as a
    /// unit, so it is already inside a transaction, and a word that
    /// began or ended one halfway through would be saying the unit is
    /// two.
    fn parse_block_part(&mut self) -> Result<(Statement, Ending)> {
        if self.at_txn_stmt() {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                self.here(),
                format_args!(
                    "a statement block runs as one transaction, so a word that begins or ends one is written on its own rather than among its parts"
                ),
            ));
        }
        if self.at_catalog_stmt() {
            let stmt = self.parse_catalog_stmt()?;
            return Ok((Statement::Catalog(stmt), Ending::Write));
        }
        let (query, ending) = self.parse_query_body()?;
        Ok((Statement::Query(query), ending))
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

    /// The variable scope clause of an inline call (ISO 13.4), which
    /// says what the block is allowed to read of the row it runs for.
    ///
    /// `None` is a block written with no clause at all and `Some` is
    /// the list, which may be empty: `CALL () { ... }` is a block that
    /// reads nothing, and it is written that way on purpose rather than
    /// by leaving the parentheses off.
    fn parse_variable_scope(&mut self) -> Result<Option<Vec<String>>> {
        if !self.eat(&TokenKind::LParen) {
            return Ok(None);
        }
        let mut names = Vec::new();
        if !self.at(&TokenKind::RParen) {
            names.push(self.expect_variable("a variable name in the scope of a CALL")?);
            while self.eat(&TokenKind::Comma) {
                names.push(self.expect_variable("a variable name in the scope of a CALL")?);
            }
        }
        self.expect(&TokenKind::RParen)?;
        Ok(Some(names))
    }

    /// The block of an inline call: a whole query between braces.
    ///
    /// Unlike the query inside a `VALUE` or an `EXISTS` it need not end
    /// with a `RETURN`, because a block that returns nothing is a
    /// perfectly good one. It says the row it ran for goes on unchanged
    /// or, where the block wrote something, that the writing happened.
    ///
    /// `head` says the call is the whole front of the statement, with
    /// nothing written before the word, which is what lets a catalog
    /// statement open the body (GP18). See `lift_catalog_out_of_a_call`
    /// for why it is only allowed there.
    fn parse_call_block(&mut self, head: bool) -> Result<(Query, Ending)> {
        self.expect(&TokenKind::LBrace)?;
        while self.at_catalog_stmt() {
            self.lift_catalog_out_of_a_call(head)?;
        }
        let (query, ending) = self.parse_query_body()?;
        self.refuse_catalog_in_a_call()?;
        self.expect(&TokenKind::RBrace)?;
        Ok((query, ending))
    }

    /// Takes a catalog statement standing at the top of a call body out
    /// of the body and puts it in front of the statement (GP18).
    ///
    /// ISO lets a procedure body hold a catalog statement among its
    /// data statements, and the corpus writes the mix inside a `CALL`,
    /// which is a procedure body too. The trouble with a call body is
    /// that it runs once for every row that reaches it, and a graph is
    /// made once or not at all, so a body that made one would work for
    /// a call over one row and raise for a call over two. A call
    /// written as the front of the statement has exactly one row to run
    /// for, the unit row every statement starts from, so there the mix
    /// means something, and `CALL { A NEXT B } rest` says what
    /// `A NEXT CALL { B } rest` says. That rewrite is what this does,
    /// which is why the lift is refused anywhere else: it is not that
    /// the standard forbids the mix further in, it is that lifting it
    /// there would move a statement out of the loop it was written in.
    fn lift_catalog_out_of_a_call(&mut self, head: bool) -> Result<()> {
        if !head {
            return self.refuse_catalog_in_a_call();
        }
        let from = self.here();
        self.parse_catalog_stmt()?;
        let to = self.here();
        if !self.eat_kw("NEXT") {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                self.here(),
                format_args!(
                    "a call body answers the rows the call adds to the row it ran for and a catalog statement answers none, so one written in a body hands over to a statement that does"
                ),
            ));
        }
        self.lifted.push(Lift {
            from,
            to,
            cut: self.here(),
        });
        Ok(())
    }

    /// Takes the body of a call the statement begins with out of the
    /// call and puts its statements in the `NEXT` chain around it,
    /// answering `None` where it did and the body back where it did
    /// not.
    ///
    /// A call adds what its body answers to the row it ran for. The
    /// row a call at the front of a statement runs for is the unit row
    /// every statement starts from, which carries nothing, so what the
    /// row becomes is what the body answered and `CALL { A } rest` says
    /// what `A NEXT rest` says. Written the second way it is a chain of
    /// statements, and a statement of a chain may do the things a
    /// spliced body may not: count its rows, order them, take a page of
    /// them, or be several statements itself.
    ///
    /// Three things hold it back. A scope clause naming variables is
    /// asking about a row that has none, and it is refused rather than
    /// quietly made to mean nothing. A body written out of set
    /// operators is not a chain to be put in one. And a body that does
    /// not end with `RETURN` answers no rows for a `NEXT` to read, so
    /// it stays where it was written.
    ///
    /// A body naming a graph of its own (GP11 through GP13) has to come
    /// out here, since a spliced body runs against the graph the
    /// statement around it does and this one does not. Its definitions
    /// come out with it, the graph clause being allowed to read one,
    /// and what that costs is checked in
    /// [`Self::parse_query_body`]: the clause governs the whole chain
    /// the statements go into, so the statements written behind the
    /// call may only project what it answered.
    fn hoist_a_leading_call(
        &mut self,
        scope: &Option<Vec<String>>,
        body: Query,
        ending: Ending,
    ) -> Option<Query> {
        if scope.as_ref().is_some_and(|names| !names.is_empty())
            || ending != Ending::Result
            // A body carrying definitions and no graph clause is left
            // where it was written, a spliced block already being able
            // to define names of its own (GP17).
            || (body.use_graph.is_none() && !body.bindings.is_empty())
        {
            return Some(body);
        }
        let Query {
            at_schema,
            use_graph,
            bindings,
            body,
        } = body;
        match body {
            Composite::Linear(linear) => {
                // The schema clause of the body becomes the schema
                // clause of the query the statements go into. There is
                // nothing to lose there: a call the statement begins
                // with is one whose word is the first token written, so
                // no clause of the query in front of it wrote one.
                self.hoisted_at = at_schema;
                self.hoisted_use = use_graph.map(|graph| (graph, linear.statements.len()));
                self.hoisted_bindings = bindings;
                self.hoisted = Some((linear.statements, ending));
                None
            }
            body => Some(Query {
                at_schema,
                use_graph,
                bindings,
                body,
            }),
        }
    }

    /// A catalog statement inside a call body is refused (GP18) where
    /// it cannot be lifted out of one.
    ///
    /// That is a call the statement does not begin with, which runs for
    /// every row that reaches it, and a catalog statement written
    /// behind the data statements of a body rather than in front of
    /// them, where taking it out would run it before the statements it
    /// was written after.
    fn refuse_catalog_in_a_call(&self) -> Result<()> {
        if !self.at_catalog_stmt() && !self.next_hands_to_catalog() {
            return Ok(());
        }
        Err(ZuError::gql_in(
            codes::C42001,
            self.source,
            self.here(),
            format_args!(
                "a call body runs once for every row that reaches it and a catalog statement makes or unmakes a thing once, so it is written beside the call, or at the front of the body of a call the statement begins with"
            ),
        ))
    }

    /// The binding variable definition block (ISO 13.3, GP17): the
    /// definitions written between the `USE` and the first statement.
    ///
    /// Three words open one and none of them can open a statement, so
    /// no lookahead past the first token is needed to know a definition
    /// is coming: nothing that runs begins with `VALUE`, `TABLE` or
    /// `GRAPH`, the statements that name a graph beginning with the
    /// verb instead. `BINDING` and `PROPERTY` are the optional long
    /// spellings the standard allows on the two reference types, and
    /// they are read the same way here as in a type.
    fn parse_binding_block(&mut self) -> Result<Vec<BindingDef>> {
        let mut out = Vec::new();
        loop {
            let kind = if self.at_kw("VALUE") {
                self.pos += 1;
                BindingKind::Value
            } else if self.at_kw("TABLE") || (self.at_kw("BINDING") && self.kw_at(1, "TABLE")) {
                self.eat_kw("BINDING");
                self.pos += 1;
                BindingKind::Table
            } else if self.at_kw("GRAPH") || (self.at_kw("PROPERTY") && self.kw_at(1, "GRAPH")) {
                self.eat_kw("PROPERTY");
                self.pos += 1;
                BindingKind::Graph
            } else {
                return Ok(out);
            };
            out.push(self.parse_binding_def(kind)?);
        }
    }

    /// One definition: the name, the type it was written with if any,
    /// and what it is initialized with.
    ///
    /// The initializer is where the three kinds part company. A brace
    /// is a query for a table and for a graph, ISO's nested binding
    /// table query and nested graph query, and is the one place a brace
    /// after an equals is not a map. A value takes an expression, which
    /// is enough for it: the query form of a value is `VALUE { ... }`,
    /// already an expression, so a value variable defined out of a
    /// query is that expression written after the equals rather than a
    /// second rule here.
    fn parse_binding_def(&mut self, kind: BindingKind) -> Result<BindingDef> {
        let name = self.expect_variable("a binding variable name")?;
        self.parse_binding_def_body(kind, name)
    }

    /// The definition behind the name, which is everything but the
    /// name: a session parameter is written with a dollar and read the
    /// same way from here on (ISO 7.1).
    fn parse_binding_def_body(&mut self, kind: BindingKind, name: String) -> Result<BindingDef> {
        // GP06, the typed definition. ISO writes the separator two
        // ways, `::` and the word `TYPED`, and allows the type to be
        // written with neither, so all three are read here and the
        // equals is what says the type is over.
        if self.at_double_colon() {
            self.pos += 2;
        } else {
            self.eat_kw("TYPED");
        }
        let ty = if self.at(&TokenKind::Eq) {
            None
        } else {
            Some(self.parse_value_type()?)
        };
        self.expect(&TokenKind::Eq)?;
        // `VALUE v = VALUE { ... }` is the standard's spelling and the
        // word is redundant here, the equals having already said a
        // definition is coming, so it is read and dropped.
        let word = kind == BindingKind::Value
            && self.at_kw("VALUE")
            && matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::LBrace)
            );
        if word {
            self.pos += 1;
        }
        // GP13. A graph expression is a graph reference or an object
        // expression, and `VALUE { ... }` is the one object expression
        // that carries a query, so it is read as an expression rather
        // than handed to the reference reader, which has no rule that
        // begins with a brace and would refuse it for the wrong
        // reason. Everything else written after a `GRAPH` names a
        // graph, so it still goes to the reference reader.
        let value_query = self.at_kw("VALUE")
            && matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::LBrace)
            );
        let init = if self.at(&TokenKind::LBrace) {
            BindingInit::Query(Box::new(self.parse_call_block(false)?.0))
        } else if kind == BindingKind::Graph && !value_query {
            BindingInit::Expr(Expr::GraphRef(self.parse_graph_ref()?))
        } else {
            BindingInit::Expr(self.parse_expr()?)
        };
        Ok(BindingDef {
            kind,
            name,
            ty,
            init,
        })
    }

    /// The body of a composite query and how its last statement ended,
    /// stopping wherever the operands run out: the `USE` in front of
    /// it, and the linear query statements it joins. What may follow is
    /// the caller's business: the end of the text for a statement, a
    /// closing brace for a nested query.
    ///
    /// The conjunctions are left associative and share one level, so
    /// this is a fold rather than a precedence climb: each operand read
    /// joins onto everything read before it.
    fn parse_query_body(&mut self) -> Result<(Query, Ending)> {
        // Anything a call body has already given up belongs to the
        // query being read around this one, which is the query this is
        // written inside rather than this one, so it is put aside for
        // the length of this read and handed back at the end.
        let waiting = (
            self.hoisted_at.take(),
            self.hoisted_use.take(),
            std::mem::take(&mut self.hoisted_bindings),
            std::mem::take(&mut self.select_body),
            self.select_graph.take(),
        );
        let read = self.parse_query_body_inner();
        (
            self.hoisted_at,
            self.hoisted_use,
            self.hoisted_bindings,
            self.select_body,
            self.select_graph,
        ) = waiting;
        read
    }

    fn parse_query_body_inner(&mut self) -> Result<(Query, Ending)> {
        // ISO 9.2 writes the schema clause in front of the graph
        // clause, and both in front of the definitions.
        let at_schema = self.parse_at_schema()?;
        let use_graph = self.parse_use_graph()?;
        let mut bindings = self.parse_binding_block()?;
        // The graph clause belongs to the statement rather than to the
        // head, so ISO lets it stand behind the definitions as well as
        // in front of them, and it has to: a clause naming a graph a
        // definition above it defined can only be written there.
        let use_graph = match (use_graph, self.here(), self.parse_use_graph()?) {
            (None, _, behind) => behind,
            (front, _, None) => front,
            (Some(_), at, Some(_)) => {
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    at,
                    format_args!(
                        "a statement runs against one graph, so it names one once: there is a USE in front of the definitions already"
                    ),
                ));
            }
        };
        let (linear, ending) = self.parse_linear()?;
        // The graph a select statement body named, which is where that
        // form writes what a `USE` clause writes in front. A statement
        // that wrote both says one graph twice or two graphs once, and
        // the second of those is the one this engine declines.
        let use_graph = match (use_graph, self.select_graph.take()) {
            (front, None) => front,
            (None, from) => from,
            (Some(front), Some(from)) if front == from => Some(front),
            (Some(_), Some(_)) => {
                return Err(ZuError::gql(
                    codes::C25G04,
                    "a statement runs against the one graph it names, and the USE clause and the select statement body name two different ones".to_string(),
                ));
            }
        };
        // The schema clause of a body taken out of a call the statement
        // begins with, which is this query's now.
        let at_schema = at_schema.or_else(|| self.hoisted_at.take());
        // The graph clause and the definitions of that body, which are
        // this query's for the same reason. The clause governs every
        // statement of the chain, so the ones written behind the call
        // may only project what it answered.
        let mut hoisted_a_use = false;
        let use_graph = match (use_graph, self.hoisted_use.take()) {
            (Some(_), Some(_)) => {
                return Err(ZuError::gql(
                    codes::C42001,
                    "the block of a CALL names a graph and so does the statement it begins, and a statement runs against one graph: take one of the two USE clauses out".to_string(),
                ));
            }
            (None, Some((graph, hoisted))) => {
                self.refuse_a_read_behind_a_hoisted_use(&linear, hoisted)?;
                bindings.append(&mut self.hoisted_bindings);
                hoisted_a_use = true;
                Some(graph)
            }
            (mine, None) => mine,
        };
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
            if hoisted_a_use {
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    at,
                    format_args!(
                        "the graph the block of the leading CALL named governs the whole statement, so there is nothing for {} to join it to: write the call as a statement of its own",
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
        Ok((
            Query {
                at_schema,
                use_graph,
                bindings,
                body,
            },
            ending,
        ))
    }

    /// Refuses a statement written behind a leading call whose body
    /// named a graph, where that statement reads the graph.
    ///
    /// `hoisted` is how many statements of the chain came out of the
    /// body. What the body named governs all of them, zu running one
    /// graph per query, and the statements behind it were written
    /// outside the call and against the graph the session is working
    /// in. A statement that only projects what the call answered
    /// cannot tell the two apart, so it is let through, and one that
    /// reads the graph is refused rather than quietly read against a
    /// graph nobody wrote it for.
    fn refuse_a_read_behind_a_hoisted_use(&self, linear: &Linear, hoisted: usize) -> Result<()> {
        if linear
            .statements
            .iter()
            .skip(hoisted)
            .all(|simple| simple.clauses.is_empty())
        {
            return Ok(());
        }
        Err(ZuError::gql(
            codes::C42001,
            "the block of the leading CALL names a graph, and the graph a statement names is the graph the whole statement runs against, so what is written behind the call may only project what the call answered: write the call as a statement of its own".to_string(),
        ))
    }

    /// A linear query statement: simple statements chained by `NEXT`,
    /// and how the last of them ended.
    fn parse_linear(&mut self) -> Result<(Linear, Ending)> {
        let mut statements = Vec::new();
        loop {
            let (simple, ending) = self.parse_simple()?;
            // The body of a call the statement began with, which goes
            // in this chain in front of what was written after the
            // call. Where the call was the whole statement what is left
            // of it is empty, and the last statement of the body is the
            // one this chain ends with.
            let (simple, ending) = match self.hoisted.take() {
                None => (simple, ending),
                Some((mut body, theirs)) => {
                    let mine = simple.clauses.is_empty() && simple.result.is_none();
                    let last = if mine {
                        body.pop().expect("a call block holds a statement")
                    } else {
                        simple
                    };
                    statements.append(&mut body);
                    (last, if mine { theirs } else { ending })
                }
            };
            statements.push(simple);
            if self.at_kw("NEXT") && ending == Ending::Finish {
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    self.peek().expect("peeked").start,
                    format_args!(
                        "NEXT reads what the statement in front of it returned, and FINISH is how a statement says it returns nothing"
                    ),
                ));
            }
            // A catalog statement past the NEXT is not part of this
            // chain, so the NEXT is left where it is for the statement
            // block to read (GP18).
            if self.next_hands_to_catalog() || !self.eat_kw("NEXT") {
                return Ok((Linear { statements }, ending));
            }
            self.refuse_a_second_graph()?;
        }
    }

    /// The graph clause a statement past a `NEXT` writes for itself.
    ///
    /// ISO 13.1 admits it: `<next statement>` hands over to a whole
    /// `<statement>`, and a focused one begins with its own `<use graph
    /// clause>`, so a chain may walk from one graph into another. This
    /// engine holds one graph open for the length of a statement, which
    /// is why the answer is 25G04 and not a syntax error: the grammar is
    /// fine and the engine is declining it.
    fn refuse_a_second_graph(&mut self) -> Result<()> {
        if !self.at_kw("USE") {
            return Ok(());
        }
        let at = self.peek().expect("peeked").start;
        Err(ZuError::gql_in(
            codes::C25G04,
            self.source,
            at,
            format_args!(
                "a statement runs against the one graph it names, and this USE names a second one for the statement past the NEXT; write the two as two statements"
            ),
        ))
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

    /// One match statement, and the yield clause that belongs to it.
    ///
    /// Two readers: the clause of a simple query statement, and the
    /// match a select statement body lists after a graph expression.
    /// ISO 14.12 says the body holds `<match statement>`, which is this
    /// same rule, so the two forms take the same patterns, the same
    /// modes and the same optional word rather than the select form
    /// taking a narrower match that happens to look like one.
    fn parse_match_clause(&mut self, clauses: &mut Vec<Clause>) -> Result<()> {
        let optional = self.eat_kw("OPTIONAL");
        // GQ21. An OPTIONAL takes either one match statement or a
        // braced block of them. The block is one operand, so it either
        // matches whole or every name it writes is null, which is why
        // the whole block becomes one bracketed group rather than a
        // group per statement.
        let (patterns, alts, distinct, filter) = if optional && self.at(&TokenKind::LBrace) {
            let (patterns, filter) = self.parse_match_block(&TokenKind::RBrace)?;
            (patterns, Vec::new(), false, filter)
        } else {
            self.expect_kw("MATCH")?;
            let (patterns, alts, bar) = self.parse_graph_pattern_alts()?;
            // A bar that was never written leaves nothing to say about
            // duplicates: the ways a quantifier spelled out are
            // lengths, and two lengths cannot be the same path.
            (patterns, alts, bar == Some(false), self.parse_where()?)
        };
        clauses.push(Clause::Match {
            optional,
            patterns,
            alts,
            distinct,
            filter,
        });
        // GQ19. A yield belongs to the match it stands after, and what
        // it does is narrow what the match wrote, so it is a clause of
        // its own here and a statement of the match in the standard.
        // Nothing may stand between the two, which is what makes the
        // two readings the same.
        if self.eat_kw("YIELD") {
            let mut items = vec![self.parse_yield_item()?];
            while self.eat(&TokenKind::Comma) {
                items.push(self.parse_yield_item()?);
            }
            clauses.push(Clause::Yield { items });
        }
        Ok(())
    }

    /// One simple query statement: the primitive statements it is
    /// written out of, and the result statement it ends with.
    fn parse_simple(&mut self) -> Result<(Simple, Ending)> {
        let mut clauses = Vec::new();
        loop {
            // An OPTIONAL takes a match or a call, so the word alone
            // does not say which clause this is: the token behind it
            // does, and a call is read where a call is read.
            if clauses.is_empty() && self.at_kw("SELECT") {
                return self.parse_select();
            } else if self.at_kw("MATCH") || (self.at_kw("OPTIONAL") && !self.kw_at(1, "CALL")) {
                self.parse_match_clause(&mut clauses)?;
            } else if self.eat_kw("INSERT") {
                let mut patterns = vec![self.parse_path()?];
                while self.eat(&TokenKind::Comma) {
                    patterns.push(self.parse_path()?);
                }
                clauses.push(Clause::Insert { patterns });
            } else if self.eat_kw("MERGE") {
                clauses.push(self.parse_merge()?);
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
            } else if self.at_kw("CALL") || self.at_kw("OPTIONAL") {
                // GP03. The word in front says the row is kept where
                // the block answers nothing, which is a thing only the
                // inline call does: a table function answers what it
                // answers and there is no row of the statement for it
                // to keep.
                let optional = self.eat_kw("OPTIONAL");
                self.expect_kw("CALL")?;
                // Which of the two calls this is, read off the one
                // token after the word. A block or a scope clause is
                // the inline call, and a name is the table function,
                // there being nothing else a call can start with.
                if self.at(&TokenKind::LBrace) || self.at(&TokenKind::LParen) {
                    // The word was the first token of the statement, so
                    // the body runs for the one row a statement starts
                    // from and a catalog statement may open it (GP18).
                    // An OPTIONAL is never that, the word being a token
                    // of its own in front: what a hoisted body answers
                    // is the statement's own rows, and there would be
                    // no row left over to keep.
                    let head = self.pos == 1;
                    let scope = self.parse_variable_scope()?;
                    let (body, ending) = self.parse_call_block(head)?;
                    let Some(body) = (if head {
                        self.hoist_a_leading_call(&scope, body, ending)
                    } else {
                        Some(body)
                    }) else {
                        continue;
                    };
                    clauses.push(Clause::CallInline {
                        optional,
                        scope,
                        body: Box::new(body),
                    });
                    continue;
                }
                if optional {
                    return Err(ZuError::gql_in(
                        codes::C42001,
                        self.source,
                        self.here(),
                        format_args!(
                            "OPTIONAL says the row is kept where what follows answers nothing, and a procedure named in the catalog answers a table of its own rather than more of the row: write the call without the word, or write the body out as a block"
                        ),
                    ));
                }
                // `AT` names the schema the reference is resolved in,
                // which is what a call written without a path in its
                // name is asking about. It comes before the name, the
                // way it does in the standard, because it is part of
                // reading the name and not part of the call.
                let (at, proc) = if self.eat_kw("AT") {
                    self.parse_at_and_name()?
                } else {
                    (None, self.parse_proc_ref()?)
                };
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
                clauses.push(Clause::Call {
                    at,
                    proc,
                    args,
                    yields,
                });
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
                let alias = self.expect_variable("a variable name after FOR")?;
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
            } else if self.at_kw("ORDER")
                || self.at_kw("OFFSET")
                || self.at_kw("SKIP")
                || self.at_kw("LIMIT")
            {
                // ISO 14.9 standing on its own. The same words after a
                // RETURN or a WITH were eaten by that projection, so
                // reaching here means there is nothing in front of them
                // but the rows themselves.
                let (keys, skip, limit) = self.parse_order_and_page()?;
                clauses.push(Clause::Order { keys, skip, limit });
            } else if self.eat_kw("FINISH") {
                // ISO 14.10. A query that ends here answers no rows and
                // no columns, which is not the same as answering
                // nothing: the clauses in front of it ran, and a write
                // in one of them wrote.
                clauses.push(Clause::Finish);
                return Ok((
                    Simple {
                        clauses,
                        result: None,
                    },
                    Ending::Finish,
                ));
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
            } else if clauses.is_empty() {
                // A select statement is a whole statement rather than a
                // clause, so the word is one of the answers here and is
                // not one anywhere else in the loop.
                return Err(
                    self.error("MATCH, OPTIONAL MATCH, SELECT, CALL, UNWIND, WITH, or RETURN")
                );
            } else {
                return Err(self.error("MATCH, OPTIONAL MATCH, CALL, UNWIND, WITH, or RETURN"));
            }
        }
    }

    /// The select statement of ISO 14.12.
    ///
    /// It is the second way GQL spells a query and it is not an
    /// optional feature: no code in `features.xml` gates it, and it
    /// hangs off `<focused linear query statement>` beside the match
    /// and return form every other query here is written in. What it
    /// says is what the other form says in a different order, the items
    /// first and the rows they come from second, so it is read into the
    /// same clauses rather than into a shape of its own. That is not a
    /// shortcut: a select statement and the linear statement below have
    /// the same answer by 14.12's own General Rules, and one shape
    /// means one binder, one optimizer and one set of conditions.
    ///
    /// ```text
    /// SELECT DISTINCT a, COUNT(*) AS n FROM g MATCH (x) WHERE p GROUP BY a HAVING n > 1 ORDER BY a
    /// USE g MATCH (x) FILTER p WITH DISTINCT a, COUNT(*) AS n, (n > 1) AS #having GROUP BY a
    ///                          FILTER #having RETURN a, n ORDER BY a
    /// ```
    ///
    /// The having clause is what makes the second line longer than the
    /// first. It tests a group rather than a row, so it has to run
    /// after the projection that made the groups and before the order
    /// and the page, and the value it tests is one the projection is
    /// the only place that can work out. So the projection carries it
    /// as a column of its own under a name no query can write, a filter
    /// behind the projection reads that column, and the result
    /// statement projects the columns the reader asked for. Where no
    /// having clause is written none of that is there and the select
    /// statement is one projection.
    fn parse_select(&mut self) -> Result<(Simple, Ending)> {
        let at = self.here();
        self.expect_kw("SELECT")?;
        // ISO 10.9. `ALL` is the default written down, so both words
        // are read and only one of them leaves a mark.
        let distinct = if self.eat_kw("DISTINCT") {
            true
        } else {
            self.eat_kw("ALL");
            false
        };
        let mut star = false;
        let mut items = Vec::new();
        if self.eat(&TokenKind::Star) {
            star = true;
        } else {
            items.push(self.parse_projection_item()?);
            while self.eat(&TokenKind::Comma) {
                items.push(self.parse_projection_item()?);
            }
        }
        let mut clauses = Vec::new();
        let (mut group_by, mut having) = (Vec::new(), None);
        let (mut order_by, mut skip, mut limit) = (Vec::new(), None, None);
        // The body and everything after it is one optional group in
        // 14.12, so `SELECT 1 AS n` is a whole statement: the items are
        // read over the one row a statement starts from, and there is
        // nothing to narrow or group.
        if self.eat_kw("FROM") {
            self.parse_select_body(&mut clauses)?;
            if let Some(expr) = self.parse_where()? {
                // The where clause sits after the body rather than
                // inside a match of it, so it narrows what the whole
                // body bound. That is a filter statement, which is the
                // clause GQL gives that job elsewhere too.
                clauses.push(Clause::Filter { expr });
            }
            if self.eat_kw("GROUP") {
                self.expect_kw("BY")?;
                loop {
                    group_by.push(self.parse_expr()?);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            if self.eat_kw("HAVING") {
                having = Some(self.parse_expr()?);
            }
            (order_by, skip, limit) = self.parse_order_and_page()?;
        }
        let Some(having) = having else {
            return Ok((
                Simple {
                    clauses,
                    result: Some(Projection {
                        distinct,
                        star,
                        items,
                        group_by,
                        order_by,
                        skip,
                        limit,
                    }),
                },
                Ending::Result,
            ));
        };
        // The names the result statement projects, read off the items
        // before the having column joins them. An item that wrote no
        // alias is named the way the binder names an unaliased return
        // item, and by the binder's own rendering rather than a second
        // one here, so the column a select statement answers is the
        // column the equivalent return answers and stays that way.
        let names: Vec<String> = items
            .iter()
            .map(|item| match (&item.alias, &item.expr) {
                (Some(alias), _) => alias.clone(),
                (None, Expr::Variable(name)) => name.clone(),
                (None, other) => crate::binder::text(other),
            })
            .collect();
        if star {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                at,
                format_args!(
                    "a having clause tests a group, and an asterisk says the columns are whatever the body bound, so there is nothing here that says what a group is: write the items out"
                ),
            ));
        }
        // The projection hands its columns on rather than answering
        // them, so each of them is named there: a name a reader wrote
        // where they wrote one, and the name the answer would have
        // carried anyway where they did not.
        for (item, name) in items.iter_mut().zip(&names) {
            item.alias = Some(name.clone());
        }
        items.push(ProjectionItem {
            expr: having,
            alias: Some(HAVING_COLUMN.to_owned()),
        });
        clauses.push(Clause::With {
            projection: Projection {
                distinct,
                star: false,
                items,
                group_by,
                order_by: Vec::new(),
                skip: None,
                limit: None,
            },
            filter: None,
        });
        clauses.push(Clause::Filter {
            expr: Expr::Variable(HAVING_COLUMN.to_owned()),
        });
        Ok((
            Simple {
                clauses,
                result: Some(Projection {
                    distinct: false,
                    star: false,
                    items: names
                        .into_iter()
                        .map(|name| ProjectionItem {
                            expr: Expr::Variable(name.clone()),
                            alias: Some(name),
                        })
                        .collect(),
                    group_by: Vec::new(),
                    order_by,
                    skip,
                    limit,
                }),
            },
            Ending::Result,
        ))
    }

    /// The select statement body of ISO 14.12: what the items are read
    /// over.
    ///
    /// Two forms. A graph match list is a graph expression and a match
    /// statement, as many times as the reader writes them, and two
    /// matches that share no variable bind every pairing of what each
    /// found, which is what two match statements written one after the
    /// other already do here. A query specification is a whole query in
    /// braces, which is the inline call this engine already has: the
    /// block answers a table and the statement goes on over its rows.
    ///
    /// The graph expression is not optional in ISO's rule, so every
    /// match names one. This engine runs a statement against the one
    /// graph the statement names, so a list naming two different graphs
    /// is 25G04 rather than a syntax error: the grammar is fine and the
    /// engine is declining it. What the list names is carried out to
    /// the query, where it meets any `USE` clause written in front of
    /// the statement.
    fn parse_select_body(&mut self, clauses: &mut Vec<Clause>) -> Result<()> {
        if self.at(&TokenKind::LBrace) {
            let body = self.parse_nested_query_named("FROM")?;
            clauses.push(Clause::CallInline {
                optional: false,
                scope: None,
                body: Box::new(body),
            });
            return Ok(());
        }
        loop {
            let at = self.here();
            let graph = self.parse_graph_ref()?;
            // A query specification may name a graph in front of the
            // braces, which is the second of its two spellings.
            if self.at(&TokenKind::LBrace) {
                self.name_the_select_graph(graph, at)?;
                let body = self.parse_nested_query_named("FROM")?;
                clauses.push(Clause::CallInline {
                    optional: false,
                    scope: None,
                    body: Box::new(body),
                });
                return Ok(());
            }
            self.name_the_select_graph(graph, at)?;
            self.select_body = true;
            let read = self.parse_match_clause(clauses);
            self.select_body = false;
            read?;
            if !self.eat(&TokenKind::Comma) {
                return Ok(());
            }
        }
    }

    /// Records the graph one select graph match named.
    ///
    /// `CURRENT_PROPERTY_GRAPH` is the graph the statement is already
    /// running against, so it names nothing new and every case can
    /// write it. Anything else is the statement's graph, and a second
    /// one that is not the same graph is refused.
    fn name_the_select_graph(&mut self, graph: GraphRef, at: usize) -> Result<()> {
        if graph == GraphRef::Current {
            return Ok(());
        }
        match &self.select_graph {
            Some(named) if *named == graph => Ok(()),
            None => {
                self.select_graph = Some(graph);
                Ok(())
            }
            Some(_) => Err(ZuError::gql_in(
                codes::C25G04,
                self.source,
                at,
                format_args!(
                    "a statement runs against the one graph it names, and this select statement body names a second one; write the two as two statements"
                ),
            )),
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
        let name = self.expect_variable("a variable after DELETE")?;
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

    /// An `AT` clause in front of a query, which says which schema a
    /// name written without a path in it is resolved in (GP16, ISO
    /// 16.1).
    ///
    /// The word also opens the schema of a named procedure call,
    /// `CALL AT /algo pagerank(...)`, and the two never collide: this
    /// one is read where a query begins and that one where a call
    /// does, which is behind the `CALL`.
    fn parse_at_schema(&mut self) -> Result<Option<SchemaRef>> {
        if !self.eat_kw("AT") {
            return Ok(None);
        }
        if self.eat_kw("CURRENT_SCHEMA") {
            return Ok(Some(SchemaRef::Current));
        }
        if self.eat_kw("HOME_SCHEMA") {
            return Ok(Some(SchemaRef::Home));
        }
        // `SCHEMA` before the path is the long spelling and says
        // nothing the path does not.
        self.eat_kw("SCHEMA");
        // The other two spellings of a relative reference, ISO 4.3.2:
        // a period is the schema the session is in and a double period
        // climbs out of it.
        if self.eat(&TokenKind::Dot) {
            return Ok(Some(SchemaRef::Current));
        }
        if self.at(&TokenKind::DotDot) {
            return Ok(Some(self.parse_relative_schema(true)?));
        }
        Ok(Some(SchemaRef::Path(self.parse_head_schema_path()?)))
    }

    /// The path an `AT` clause at the head of a query names.
    ///
    /// Whitespace is nothing to the lexer, so `AT / MATCH (p)` and
    /// `AT /MATCH (p)` are the same tokens and the path would swallow
    /// the word the query begins with, which is the ambiguity
    /// [`Self::parse_at_and_name`] meets at a call and settles by what
    /// follows. Here the word itself settles it: a segment that opens
    /// a clause was never a segment, so the path ends in front of it
    /// and the root is what a clause word straight after the slash
    /// leaves behind. A schema really called `MATCH` is still
    /// reachable, written in quotes, since a quoted name is a name and
    /// nothing else.
    fn parse_head_schema_path(&mut self) -> Result<String> {
        if !self.eat(&TokenKind::Slash) {
            return Err(self.error("an absolute directory path"));
        }
        let mut path = String::new();
        while self.at_path_segment(0) {
            path.push('/');
            path.push_str(&self.expect_name("a name in a path")?);
            // A slash with no segment behind it ends the path and
            // belongs to whatever follows, so it is taken only when
            // one does.
            if !self.at(&TokenKind::Slash) || !self.at_path_segment(1) {
                break;
            }
            self.pos += 1;
        }
        Ok(if path.is_empty() {
            "/".to_string()
        } else {
            path
        })
    }

    /// Whether the token this many ahead can be a segment of the path
    /// above: a name that is not one of the words a clause begins
    /// with. A quoted name is always one, which is how a schema named
    /// after a keyword is written.
    fn at_path_segment(&self, ahead: usize) -> bool {
        match self.tokens.get(self.pos + ahead).map(|t| &t.kind) {
            Some(TokenKind::QuotedIdent(_)) => true,
            Some(TokenKind::Ident(word)) => !opens_a_clause(word),
            _ => false,
        }
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
        let (order_by, skip, limit) = self.parse_order_and_page()?;
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

    /// The order by and page of ISO 14.9: the sort keys, the offset and
    /// the limit, each optional and in that order.
    ///
    /// One reader of this is the tail of a projection and the other is
    /// the statement that is nothing but this, so the three parts are
    /// read in one place. Nothing here decides whether writing none of
    /// them is allowed, because after a `RETURN` it is and on its own
    /// it is not.
    #[allow(clippy::type_complexity)]
    fn parse_order_and_page(&mut self) -> Result<(Vec<SortKey<Expr>>, Option<Expr>, Option<Expr>)> {
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
        Ok((order_by, skip, limit))
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
        let mut bar = None;
        let mut alts = self.parse_path_alts(&mut bar)?;
        if alts.len() > 1 {
            return Err(ZuError::gql(codes::C42001, one_shape(bar.is_some())));
        }
        Ok(alts.remove(0))
    }

    /// One path pattern, as the terms the bars separated it into (ISO
    /// 16.7).
    ///
    /// The path variable, the selector and the path mode are written
    /// once in front of the whole alternation and belong to every term
    /// of it, because they say what to do with the path that was
    /// matched and not which shape matched it. `bar` carries which of
    /// the two bars this list has seen so far, so that a list written
    /// with both is caught wherever the second one is.
    fn parse_path_alts(&mut self, bar: &mut Option<bool>) -> Result<Vec<PathPattern>> {
        // `p = (a)-...` binds the path; the lookahead keeps a bare
        // pattern unambiguous.
        let var = if matches!(self.peek().map(|t| &t.kind), Some(TokenKind::Ident(_)))
            && matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::Eq)
            ) {
            let name = self.expect_variable("a path variable")?;
            self.expect(&TokenKind::Eq)?;
            Some(name)
        } else {
            None
        };
        let (selector, mode) = self.parse_path_prefix()?;
        let mut alts = self.parse_term(var.clone(), selector, mode)?;
        while self.at(&TokenKind::Pipe) {
            let at = self.peek().expect("peeked").start;
            self.expect(&TokenKind::Pipe)?;
            // `|+|` is three tokens rather than one, because the lexer
            // would have to know it was reading a pattern to make it
            // one: the same three characters are a bar, a plus and a
            // bar in an expression.
            let multiset = self.eat(&TokenKind::Plus);
            if multiset {
                self.expect(&TokenKind::Pipe)?;
            }
            if bar.is_some_and(|seen| seen != multiset) {
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    at,
                    "one bar answers a path once however many alternatives matched it \
                     and the other answers it once per alternative, so a pattern \
                     written with both is asking for two answers at once; write the \
                     alternatives with one of them",
                ));
            }
            *bar = Some(multiset);
            let more = self.parse_term(var.clone(), selector, mode)?;
            alts.extend(more);
        }
        Ok(alts)
    }

    /// One term of a path pattern, as the walks it stands for (ISO
    /// 16.11, features G037 and G061).
    ///
    /// A term is one walk unless it holds a stretch that repeats a
    /// variable number of times, and such a stretch is a pattern per
    /// length rather than one pattern: `((x)-[:LINK]->(y))?` is the
    /// stretch walked once and the stretch skipped, which are two
    /// shapes with different numbers of elements in them. So the term
    /// is read once per length, and once per combination of lengths
    /// where it holds more than one such stretch, and what comes back
    /// is a list of walks the way an alternation's is.
    ///
    /// The lengths are found by reading the term through: the pass that
    /// discovers them takes the least length of each stretch and writes
    /// down the range, and the passes after it are the same tokens read
    /// again with the choices made. Reading a term twice is safe
    /// because nothing but the position moves while one is read, and it
    /// is the shortest way to say this: how many nodes a stretch
    /// contributes decides where every position behind it lands, so a
    /// term of several lengths cannot be built once and patched.
    fn parse_term(
        &mut self,
        var: Option<String>,
        selector: Option<Selector>,
        mode: Option<PathMode>,
    ) -> Result<Vec<PathPattern>> {
        let from = self.pos;
        let held = std::mem::take(&mut self.spans);
        let chosen = std::mem::take(&mut self.lengths);
        let taken = std::mem::replace(&mut self.taken, 0);
        let first = self.one_term(var.clone(), selector, mode);
        let spans = std::mem::replace(&mut self.spans, held);
        self.lengths = chosen;
        self.taken = taken;
        let first = first?;
        if spans.is_empty() {
            return Ok(vec![first]);
        }
        let to = self.pos;
        let mut out = Vec::new();
        for lengths in Self::choices(&spans) {
            self.pos = from;
            let held = std::mem::take(&mut self.spans);
            let chosen = std::mem::replace(&mut self.lengths, lengths);
            let taken = std::mem::replace(&mut self.taken, 0);
            let walk = self.one_term(var.clone(), selector, mode);
            self.spans = held;
            self.lengths = chosen;
            self.taken = taken;
            out.push(walk?);
        }
        self.pos = to;
        Ok(out)
    }

    /// Every combination of lengths a term's stretches may take, in
    /// order, the last stretch counting fastest.
    fn choices(spans: &[(usize, usize)]) -> Vec<Vec<usize>> {
        let mut out: Vec<Vec<usize>> = vec![Vec::new()];
        for &(lo, hi) in spans {
            out = out
                .into_iter()
                .flat_map(|so_far| {
                    (lo..=hi).map(move |count| {
                        let mut lengths = so_far.clone();
                        lengths.push(count);
                        lengths
                    })
                })
                .collect();
        }
        out
    }

    /// One walk of a term, under the variable, selector and mode the
    /// alternation around it was written with.
    fn one_term(
        &mut self,
        var: Option<String>,
        selector: Option<Selector>,
        mode: Option<PathMode>,
    ) -> Result<PathPattern> {
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
            // A simplified path pattern is as many steps as its labels
            // spell, so the nodes between them are written here: they
            // are nodes the query did not write and says nothing about,
            // and the last step reaches the factor that follows.
            if self.at_simplified() {
                let rels = self.parse_simplified()?;
                let Some((last, rest)) = rels.split_last() else {
                    let mut right = Segment::default();
                    self.parse_factor(&mut right)?;
                    into.juxtapose(right);
                    continue;
                };
                for rel in rest {
                    let mut between = Segment::default();
                    between.join(NodePattern::default());
                    into.step(rel.clone(), between);
                }
                let mut right = Segment::default();
                self.parse_factor(&mut right)?;
                into.step(last.clone(), right);
                continue;
            }
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
            let name = self.expect_variable("a subpath variable")?;
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
        // A questioned stretch is walked once or skipped, which is the
        // range the question mark spells (ISO 16.11, feature G037), so
        // it lands in the same range every other quantifier does.
        let times = if self.eat(&TokenKind::Question) {
            Some((Some(0), Some(1)))
        } else {
            self.parse_edge_quantifier()?
        };
        let Some(times) = times else {
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
        let count = self.repetitions(times, close)?;
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
        if count == 0 {
            // A stretch walked no times is the one node its ends meet
            // at: what stood in front of the brackets goes on from
            // there and what follows them is written against it, which
            // is what a factor with nothing in it does anyway. The
            // names inside the brackets bound nothing, and they are
            // recorded above as groups with no repetition in them so
            // that this way of matching carries the same names as the
            // ways that walked the stretch.
            if into.nodes.is_empty() {
                into.join(NodePattern::default());
            }
            return Ok(());
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

    /// How many times a quantifier on a stretch repeats it in the pass
    /// in hand.
    ///
    /// A stretch repeated a fixed number of times is a longer pattern
    /// of the same shape, which is what the parser writes it out as. A
    /// stretch repeated a variable number of times is not one pattern
    /// at all: it matches paths of several lengths, so it stands for as
    /// many patterns as the range holds and what the query asked for is
    /// their union. Those are read a length at a time, and this hands
    /// out the length the pass in hand chose, writing the range down so
    /// that the term is read again for the lengths still to come.
    ///
    /// A count with no ceiling on it is refused. The lengths are
    /// written out as patterns of their own, and there is no writing
    /// out a list with no end to it, so `+`, `*` and `{n,}` on a
    /// stretch are refused by name rather than answered for as many
    /// lengths as the engine felt like walking.
    fn repetitions(&mut self, times: (Option<u64>, Option<u64>), at: usize) -> Result<usize> {
        let (lo, hi) = match times {
            // A range with no floor written starts at nought, which is
            // the stretch not walked at all.
            (min, Some(max)) if min.unwrap_or(0) <= max => {
                (min.unwrap_or(0) as usize, max as usize)
            }
            (Some(min), Some(max)) => {
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    at,
                    format!(
                        "a stretch repeated between {min} and {max} times is asking for \
                         a count above its own ceiling, which no path has; write the \
                         smaller number first"
                    ),
                ));
            }
            _ => {
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    at,
                    "a stretch repeated with no ceiling on the count matches paths of \
                     every length, and the lengths are walked as patterns of their own, \
                     so there is no list of them to walk; write a ceiling on the count",
                ));
            }
        };
        if lo == hi {
            return Ok(lo);
        }
        let at = self.taken;
        self.taken += 1;
        self.spans.push((lo, hi));
        Ok(self.lengths.get(at).copied().unwrap_or(lo))
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
    /// A graph pattern where nothing reads an alternation, which is
    /// every pattern outside a match: an `INSERT` writes one shape and
    /// a `MERGE` looks for one.
    fn parse_graph_pattern(&mut self) -> Result<Vec<PathPattern>> {
        let (patterns, alts, bar) = self.parse_graph_pattern_alts()?;
        if !alts.is_empty() {
            return Err(ZuError::gql(codes::C42001, one_shape(bar.is_some())));
        }
        Ok(patterns)
    }

    /// A graph pattern, as the ways of matching it was written as (ISO
    /// 16.3 and 16.7, features G030 and G032).
    ///
    /// The bar binds tighter than the comma, because the standard puts
    /// the alternation inside one path pattern and the comma between
    /// two of them: `A | B, C` is a list of two patterns whose first
    /// one is written two ways, so the ways of matching the list are
    /// `A, C` and `B, C`. That is what this answers, one whole list per
    /// way, since what the clause does with them is answer the rows of
    /// each in turn and there is nothing left for an operator between
    /// them to do.
    ///
    /// The two bars are not mixed in one list. `|` answers a path once
    /// however many alternatives matched it and `|+|` answers it once
    /// per alternative, so a list written with both is asking for two
    /// different answers at once.
    /// Whether the comma in hand ends a match statement of a select
    /// statement body rather than joining another pattern to the list
    /// this match is reading.
    ///
    /// ISO's own grammar reaches for the same comma twice: a select
    /// graph match list writes commas between graph matches and a path
    /// pattern list writes them between patterns, so `MATCH (a), g
    /// MATCH (b)` is two readings until the second MATCH arrives. What
    /// settles it is what stands after the comma, so this reads a graph
    /// expression and a match ahead of it and puts the tokens back. A
    /// path pattern never begins with a name that a MATCH follows,
    /// which is what makes the lookahead an answer rather than a guess,
    /// and outside a select body the question is not asked at all.
    fn comma_ends_a_select_graph_match(&mut self) -> bool {
        if !self.select_body {
            return false;
        }
        let at = self.pos;
        self.pos += 1;
        let ends = self.parse_graph_ref().is_ok()
            && (self.at_kw("MATCH") || (self.at_kw("OPTIONAL") && self.kw_at(1, "MATCH")));
        self.pos = at;
        ends
    }

    fn parse_graph_pattern_alts(&mut self) -> Result<Ways> {
        let list = PatternList {
            mode: self.parse_match_mode(),
            at: self.lists,
        };
        self.lists += 1;
        let mut bar: Option<bool> = None;
        let mut written = vec![self.parse_path_alts(&mut bar)?];
        while self.at(&TokenKind::Comma) && !self.comma_ends_a_select_graph_match() {
            self.pos += 1;
            written.push(self.parse_path_alts(&mut bar)?);
        }
        let mut ways = self.spread(written)?;
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
            for patterns in &mut ways {
                Self::keep(patterns, selector, mode)?;
            }
        }
        for patterns in &mut ways {
            for path in patterns.iter_mut() {
                path.list = list;
            }
        }
        let first = ways.remove(0);
        Ok((first, ways, bar))
    }

    /// The ways of matching a whole list, out of the ways each pattern
    /// in it was written.
    ///
    /// A list is a join, so a list whose patterns were written two and
    /// three ways is six ways of matching and every one of them is a
    /// list of its own. That multiplies, which is why there is a
    /// ceiling on it: each way is a walk the engine runs, and a
    /// statement asking for more walks than this is describing
    /// something a reader of it could not hold in their head either.
    fn spread(&self, written: Vec<Vec<PathPattern>>) -> Result<Vec<Vec<PathPattern>>> {
        const CEILING: usize = 64;
        let ways = written.iter().map(Vec::len).product::<usize>();
        if ways > CEILING {
            return Err(ZuError::gql(
                codes::C42001,
                format!(
                    "the patterns of this list are written {ways} ways between them, \
                     and each way is a walk of its own; write the alternatives as \
                     separate statements joined with UNION, which says the same thing \
                     and says how many walks it is asking for"
                ),
            ));
        }
        let mut ways: Vec<Vec<PathPattern>> = vec![Vec::new()];
        for alts in written {
            ways = ways
                .into_iter()
                .flat_map(|so_far| {
                    alts.iter().map(move |path| {
                        let mut list = so_far.clone();
                        list.push(path.clone());
                        list
                    })
                })
                .collect();
        }
        Ok(ways)
    }

    /// Fills in what the patterns of a list left out, which is what a
    /// `KEEP` behind the list is for.
    fn keep(
        patterns: &mut [PathPattern],
        selector: Option<Selector>,
        mode: Option<PathMode>,
    ) -> Result<()> {
        for path in patterns {
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
        Ok(())
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
            // A delimited identifier is read here and refused there,
            // rather than left to fall through to the closing bracket:
            // nothing else may stand in this slot, so a query that
            // wrote one gets told the rule it broke.
            Some(TokenKind::Ident(_) | TokenKind::QuotedIdent(_) | TokenKind::Str(_)) => {
                Some(self.expect_variable("a variable")?)
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
                // A delimited identifier is read here and refused
                // there, rather than left to fall through to the
                // bracket: nothing else may stand in this slot, so a
                // query that wrote one gets told the rule it broke.
                Some(TokenKind::Ident(_) | TokenKind::QuotedIdent(_) | TokenKind::Str(_)) => {
                    Some(self.expect_variable("a variable")?)
                }
                _ => None,
            };
            let mut types = Vec::new();
            if self.eat(&TokenKind::Colon) {
                types.push(self.expect_name("a relationship type")?);
                while self.eat(&TokenKind::Pipe) {
                    types.push(self.expect_name("a relationship type")?);
                }
                self.refuse_edge_conjunction()?;
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
        let Some(direction) = a_direction(inbound, left_tilde, outbound) else {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                self.tokens[self.pos - 1].start,
                "an undirected relationship cannot point both ways",
            ));
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

    /// Whether an arrow opens a simplified path pattern (ISO 16.12),
    /// which is told from an ordinary one by the slash: the label
    /// expression goes inside the arrow rather than in brackets behind
    /// it, so `-/ LINK /->` is where `-[:LINK]->` would have stood.
    fn at_simplified(&self) -> bool {
        let mut at = self.pos;
        if matches!(self.tokens.get(at).map(|t| &t.kind), Some(TokenKind::Lt)) {
            at += 1;
        }
        if !matches!(
            self.tokens.get(at).map(|t| &t.kind),
            Some(TokenKind::Minus | TokenKind::Tilde)
        ) {
            return false;
        }
        matches!(
            self.tokens.get(at + 1).map(|t| &t.kind),
            Some(TokenKind::Slash)
        )
    }

    /// A simplified path pattern (ISO 16.12, features G039 and G080 to
    /// G082): the steps it describes, in written order.
    ///
    /// What the slashes hold is an expression over edge labels rather
    /// than one label: labels written one after another are steps one
    /// after another, a bar between them is either label on the one
    /// step, brackets group, and a quantifier behind any of it repeats
    /// it. The arrow around the slashes says which way the steps go,
    /// and a step may write a direction of its own in front of or
    /// behind its label, which is what the override features are.
    ///
    /// It reads as the pattern it abbreviates and nothing below the
    /// parser learns about it: `-/ A B /->` is `-[:A]->()-[:B]->`, and
    /// the anonymous node between the steps is written by the caller,
    /// which is the one that knows what the pattern ends at.
    fn parse_simplified(&mut self) -> Result<Vec<RelPattern>> {
        let inbound = self.eat(&TokenKind::Lt);
        let left_tilde = self.parse_rel_bar()?;
        self.expect(&TokenKind::Slash)?;
        let steps = self.parse_simple_contents()?;
        let at = self.peek().map(|t| t.start).unwrap_or(self.source.len());
        self.expect(&TokenKind::Slash)?;
        let right_tilde = self.parse_rel_bar()?;
        if left_tilde != right_tilde {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                at,
                "a simplified path pattern is undirected at both ends or at neither",
            ));
        }
        let outbound = self.eat(&TokenKind::Gt);
        let Some(default) = a_direction(inbound, left_tilde, outbound) else {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                at,
                "an undirected relationship cannot point both ways",
            ));
        };
        Ok(steps
            .into_iter()
            .map(|step| RelPattern {
                var: None,
                types: step.types,
                direction: step.direction.unwrap_or(default),
                range: step.range,
                props: Vec::new(),
                filter: None,
            })
            .collect())
    }

    /// What the slashes hold: the terms and the bars between them.
    fn parse_simple_contents(&mut self) -> Result<Vec<Simplified>> {
        let at = self.peek().map(|t| t.start).unwrap_or(self.source.len());
        let mut ways = vec![self.parse_simple_term()?];
        let mut multiset = false;
        while self.eat(&TokenKind::Pipe) {
            if self.eat(&TokenKind::Plus) {
                multiset = true;
                self.expect(&TokenKind::Pipe)?;
            }
            ways.push(self.parse_simple_term()?);
        }
        if ways.len() == 1 {
            return Ok(ways.pop().expect("a term was read"));
        }
        if multiset {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                at,
                "a multiset alternation answers a path once per way that found it, and \
                 the ways inside a simplified path pattern are labels on one step \
                 rather than walks of their own; write the alternatives as path \
                 patterns with a bar between them",
            ));
        }
        // Two ways of one step each are the one step with either label
        // on it, which is what a step written `[:A|B]` is. Ways of more
        // than one step are walks of different shapes, and a walk is
        // not something a label expression can hold.
        let mut types = Vec::new();
        for way in &ways {
            match way.as_slice() {
                [one] if one.range.is_none() && one.direction == ways[0][0].direction => {
                    // A label written on two of the ways is the one
                    // label: what the bar answers is a set of paths,
                    // and a step that walked the same edge under the
                    // same type walked the same path.
                    for name in &one.types {
                        if !types.contains(name) {
                            types.push(name.clone());
                        }
                    }
                }
                _ => {
                    return Err(ZuError::gql_in(
                        codes::C42001,
                        self.source,
                        at,
                        "a bar inside a simplified path pattern says either label on the \
                         one step, so the ways it separates are single steps that go the \
                         same way; write the alternatives as path patterns with a bar \
                         between them",
                    ));
                }
            }
        }
        Ok(vec![Simplified {
            types,
            direction: ways[0][0].direction,
            range: None,
        }])
    }

    /// One term: the factors written one after another, which are the
    /// steps of the walk in the order it takes them.
    fn parse_simple_term(&mut self) -> Result<Vec<Simplified>> {
        let mut out = self.parse_simple_factor()?;
        while self.at_simple_factor() {
            out.extend(self.parse_simple_factor()?);
        }
        Ok(out)
    }

    /// Whether another factor is written here, which is what ends a
    /// term: a bar, a closing bracket and the closing slash do not.
    fn at_simple_factor(&self) -> bool {
        matches!(
            self.peek().map(|t| &t.kind),
            Some(
                TokenKind::Ident(_)
                    | TokenKind::QuotedIdent(_)
                    | TokenKind::LParen
                    | TokenKind::Lt
                    | TokenKind::Tilde
                    | TokenKind::Bang
                    | TokenKind::Percent
            )
        )
    }

    /// One factor: a direction of its own, then the label or the
    /// bracketed group it applies to, then a quantifier.
    fn parse_simple_factor(&mut self) -> Result<Vec<Simplified>> {
        let at = self.peek().map(|t| t.start).unwrap_or(self.source.len());
        let inbound = self.eat(&TokenKind::Lt);
        let tilde = self.eat(&TokenKind::Tilde);
        let mut steps = self.parse_simple_primary()?;
        let outbound = self.eat(&TokenKind::Gt);
        if inbound || tilde || outbound {
            let Some(over) = a_direction(inbound, tilde, outbound) else {
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    at,
                    "an undirected relationship cannot point both ways",
                ));
            };
            // A group takes the direction written on it wherever it did
            // not write one of its own, which is what makes the
            // override a default of its own rather than a rewrite.
            for step in &mut steps {
                step.direction.get_or_insert(over);
            }
        }
        let times = if self.eat(&TokenKind::Question) {
            Some((Some(0), Some(1)))
        } else {
            self.parse_edge_quantifier()?
        };
        let Some(times) = times else {
            return Ok(steps);
        };
        // A quantifier on one step that walks at least one hop is the
        // hops that step walks, which is the shape a variable-length
        // step already has, so a count with no ceiling on it costs
        // nothing here. Everything else is a stretch repeated: more
        // than one step, or a count that may be nought, which is a
        // stretch that may not be walked at all. Those are lengths, and
        // the lengths are patterns of their own.
        if let [one] = steps.as_slice()
            && one.range.is_none()
            && times.0.unwrap_or(0) >= 1
        {
            return Ok(vec![Simplified {
                types: one.types.clone(),
                direction: one.direction,
                range: Some(times),
            }]);
        }
        let count = self.repetitions(times, at)?;
        let mut out = Vec::with_capacity(steps.len() * count);
        for _ in 0..count {
            out.extend(steps.iter().cloned());
        }
        Ok(out)
    }

    /// Refuses a conjunction of edge types, which is 42007.
    ///
    /// An edge is stored under one type here, so the edge label set of
    /// an edge this engine holds has exactly one label in it. A step
    /// written `[:A&B]` asks for an edge with two, which is over the
    /// maximum in ISO 24.5.2 IL001, and 42007 is that condition seen
    /// while the statement is being read rather than while an element
    /// is being built. Read after the bars, so `[:A|B&C]` is refused
    /// for the conjunction rather than accepted as two alternatives
    /// with a stray token after them.
    fn refuse_edge_conjunction(&mut self) -> Result<()> {
        if !self.at(&TokenKind::Amp) {
            return Ok(());
        }
        let at = self.peek().map(|t| t.start).unwrap_or(self.source.len());
        let mut names = 2;
        while self.eat(&TokenKind::Amp) {
            self.expect_name("an edge type")?;
            names += 1;
        }
        Err(ZuError::gql_in(
            codes::C42007,
            self.source,
            at,
            format_args!(
                "an edge is stored under one type in this engine, so its label set holds \
                 one label and this step names {}; write the one type the step walks",
                names - 1
            ),
        ))
    }

    /// One label, or a bracketed expression over labels.
    fn parse_simple_primary(&mut self) -> Result<Vec<Simplified>> {
        let at = self.peek().map(|t| t.start).unwrap_or(self.source.len());
        if self.at(&TokenKind::Bang) || self.at(&TokenKind::Percent) {
            return Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                at,
                "an edge is stored under one type in this engine, so a label expression \
                 on a step is the types it may have and nothing else; write the types \
                 the step walks with bars between them",
            ));
        }
        if self.eat(&TokenKind::LParen) {
            let inner = self.parse_simple_contents()?;
            self.expect(&TokenKind::RParen)?;
            return Ok(inner);
        }
        let name = self.expect_name("an edge type")?;
        self.refuse_edge_conjunction()?;
        Ok(vec![Simplified {
            types: vec![name],
            direction: None,
            range: None,
        }])
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
                let key = self.expect_field_name("a property name")?;
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

    /// The `[a, b, c]` of a list constructor and of a path
    /// constructor, brackets and all.
    ///
    /// ISO calls the inside of it an element list and lets it be
    /// empty, which is how `[]` is the empty list rather than a
    /// syntax error.
    fn parse_bracketed_items(&mut self) -> Result<Vec<Expr>> {
        self.expect(&TokenKind::LBracket)?;
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
        Ok(items)
    }

    // Expressions, precedence low to high. Every recursion into a
    // subexpression goes through this depth guard.

    fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_expr_in(false)
    }

    /// An expression, and whether an `IN` at the top of it closes a
    /// `LET` expression's definitions. Every nested expression goes
    /// through here with that off, which is what keeps the word an
    /// operator everywhere except the one place it is a separator.
    fn parse_expr_in(&mut self, ends_a_definition: bool) -> Result<Expr> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(ZuError::gql(
                codes::C42001,
                format!("expression nesting deeper than {MAX_DEPTH}"),
            ));
        }
        let outer = std::mem::replace(&mut self.ends_a_definition, ends_a_definition);
        let result = self.parse_or();
        self.ends_a_definition = outer;
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
                    if !self.ends_a_definition && self.eat_kw("IN") {
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
        // ISO 19.7. The form stands in front of NORMALIZED and is
        // optional, NFC being what leaving it out means. A word that
        // names a form and has no NORMALIZED behind it belongs to
        // whatever comes next rather than to this predicate, so nothing
        // is taken on the strength of the form alone.
        if self.at_normal_form() && self.kw_at(1, "NORMALIZED") {
            let form = self.parse_normal_form()?;
            self.expect_kw("NORMALIZED")?;
            return Ok(Expr::IsNormalized {
                expr: Box::new(lhs),
                form,
                negated,
            });
        }
        if self.eat_kw("NORMALIZED") {
            return Ok(Expr::IsNormalized {
                expr: Box::new(lhs),
                form: NormalForm::Nfc,
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
        // ISO 20.20's boolean test. The three truth words are read
        // here and not as an operator of their own because IS takes
        // them all, and the word behind it is the whole of the
        // difference: NULL asks whether there is a value at all and
        // TRUE asks which one it is.
        for truth in [TruthValue::True, TruthValue::False, TruthValue::Unknown] {
            if self.eat_kw(truth.word()) {
                return Ok(Expr::BooleanTest {
                    expr: Box::new(lhs),
                    truth,
                    negated,
                });
            }
        }
        if !self.eat_kw("NULL") {
            return Err(self.error("NULL, TRUE, FALSE, or UNKNOWN"));
        }
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
            let key = self.expect_field_name("a property name after '.'")?;
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
            TokenKind::Bytes(b) => {
                self.pos += 1;
                Ok(Expr::Literal(Literal::Bytes(b)))
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
            TokenKind::LBracket => Ok(Expr::List(self.parse_bracketed_items()?)),
            TokenKind::LBrace => {
                let props = self.parse_property_map()?;
                Ok(Expr::Map(props))
            }
            // GE01. A path where a value goes names a graph, and a
            // slash can begin nothing else here: division wants a
            // value in front of it and an expression starts with none.
            //
            // The path is the whole of how a graph is named here. A
            // `USE` also takes the name on its own and takes `GRAPH`
            // in front of it, and neither of those can be read in an
            // expression: a bare name is a variable, and a word with a
            // path behind it is a division as often as it is a graph,
            // since `graph / n` is one and `graph /n` is the other and
            // no reader can see the difference.
            TokenKind::Slash => Ok(Expr::GraphRef(GraphRef::Named(self.parse_graph_name()?))),
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

    /// The parameter behind `GRAPH`, `PROPERTY GRAPH`, `TABLE` or
    /// `BINDING TABLE`, the word already read, and `None` when what
    /// stands there is not one of those words with a parameter behind
    /// it.
    ///
    /// The word is read and thrown away on purpose. It says which of
    /// the two reference types the caller means to have passed, the
    /// value says the same thing, and where they disagree the value is
    /// the one that is true, so keeping the word would only be a
    /// second place for the answer to come from.
    fn reference_parameter(&mut self, name: &str) -> Option<String> {
        // How many words of the spelling are still ahead of the
        // parameter: none for the short ones, one for each long one.
        let long = (name.eq_ignore_ascii_case("PROPERTY") && self.at_kw("GRAPH"))
            || (name.eq_ignore_ascii_case("BINDING") && self.at_kw("TABLE"));
        let short = name.eq_ignore_ascii_case("GRAPH") || name.eq_ignore_ascii_case("TABLE");
        if !long && !short {
            return None;
        }
        let words = usize::from(long);
        let Some(Token {
            kind: TokenKind::Param(param),
            ..
        }) = self.tokens.get(self.pos + words)
        else {
            return None;
        };
        let param = param.clone();
        self.pos += words + 1;
        Some(param)
    }

    /// Whether a definition stands here: a name with an equals sign
    /// behind it, which is what tells `LET` the expression from `let`
    /// the variable.
    fn at_definition(&self) -> bool {
        let name = matches!(
            self.peek().map(|t| &t.kind),
            Some(TokenKind::Ident(_) | TokenKind::QuotedIdent(_))
        );
        name && self.tokens.get(self.pos + 1).map(|t| &t.kind) == Some(&TokenKind::Eq)
    }

    /// `LET n = a + b IN n * n END`, GE03, the word already read.
    ///
    /// The definitions are read left to right and the `END` is not
    /// optional, which is what keeps the body from swallowing whatever
    /// the expression is written inside: without it `LET n = 1 IN n, 2`
    /// would be one projection item or two depending on where the
    /// reader stopped.
    fn parse_let_expr(&mut self) -> Result<Expr> {
        let mut definitions = Vec::new();
        loop {
            let name = self.expect_name("a name after LET")?;
            self.expect(&TokenKind::Eq)?;
            let expr = self.parse_expr_in(true)?;
            definitions.push(LetItem { name, expr });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect_kw("IN")?;
        let body = Box::new(self.parse_expr()?);
        self.expect_kw("END")?;
        Ok(Expr::Let { definitions, body })
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
        // ISO 21.2's third truth value. It is the null of the boolean
        // type and not a fourth kind of value, so it reads as a null
        // here: `UNKNOWN AND TRUE` is unknown for the same reason
        // `NULL AND TRUE` is, and one three valued table answers both.
        if name.eq_ignore_ascii_case("unknown") {
            return Ok(Expr::Literal(Literal::Null));
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
        // GE03, a name that lives for the length of one expression.
        // `LET` stays an ordinary variable name until a definition
        // follows it, which is a name with an equals sign behind it,
        // and nothing an expression may put after a variable read
        // begins that way: two names against each other are not an
        // expression at all. So `RETURN let` reads a variable and
        // `RETURN LET n = 1 IN n END` reads this.
        if name.eq_ignore_ascii_case("LET") && self.at_definition() {
            return self.parse_let_expr();
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
            return Ok(Expr::Path(self.parse_bracketed_items()?));
        }
        // ISO 20.3, the other half of <general value specification>.
        // The word names the principal the session was opened for, and
        // it is a reserved word, so no query can have meant a variable
        // by it. This engine authenticates nobody, so what it names is
        // the null value: ID061 says the declared type is the character
        // string and says the value is always absent, which is a thing
        // a program can read and act on where a refusal is not.
        // A bracket behind it is left to the call path, so the word
        // written as a call is an unknown function rather than a
        // puzzle about the bracket, which is how the bare temporal
        // words are refused too.
        if name.eq_ignore_ascii_case("SESSION_USER") && !self.at(&TokenKind::LParen) {
            return Ok(Expr::SessionUser);
        }
        // GE01, a graph named where a value goes. Each of these four
        // words is a whole graph reference on its own, so there is
        // nothing to read behind them and nothing they could be
        // mistaken for: a variable of one of these names is the one
        // thing ISO does reserve here.
        if name.eq_ignore_ascii_case("CURRENT_GRAPH")
            || name.eq_ignore_ascii_case("CURRENT_PROPERTY_GRAPH")
        {
            return Ok(Expr::GraphRef(GraphRef::Current));
        }
        if name.eq_ignore_ascii_case("HOME_GRAPH")
            || name.eq_ignore_ascii_case("HOME_PROPERTY_GRAPH")
        {
            return Ok(Expr::GraphRef(GraphRef::Home));
        }
        // ISO 20.27 and 20.29, the temporal value functions. Where the
        // brackets may stand is part of each one, which is why they are
        // read here rather than resolved by name: CURRENT_DATE takes
        // none, DATE(...) takes them, and LOCAL_TIME takes them or not.
        // Reading them here takes nothing away from a query, since
        // every one of the words is reserved and a variable of one of
        // those names is a variable the standard does not allow.
        //
        // A word written with brackets it does not take, or without
        // brackets it needs, falls through to the call path on purpose.
        // CURRENT_DATE() is a query asking for a function that does not
        // exist, and the refusal that says so by name reads better than
        // one about a bracket that could not go where it was put; a
        // bare DATE is a name, and stays one, the way it is a name in
        // front of a date string.
        if let Some(func) = temporal_word(&name)
            && match func.brackets() {
                Brackets::Never => !self.at(&TokenKind::LParen),
                Brackets::Always => self.at(&TokenKind::LParen),
                Brackets::Optional => true,
            }
        {
            return self.parse_temporal_fn(func);
        }
        // ISO 20.28, the datetime subtraction. It looks like a call
        // until the closing bracket, and then a qualifier may follow
        // where a call has nothing, so the call path cannot read it and
        // the name is caught here instead.
        if self.at(&TokenKind::LParen) && name.eq_ignore_ascii_case("duration_between") {
            return self.parse_duration_between();
        }
        // GE01 and GE02, the reference a caller passed in. `GRAPH $g`
        // and `BINDING TABLE $t` say what the parameter holds and
        // nothing else, which is what `USE GRAPH $g` beside `USE $g`
        // already says: the word is a reader's note, and the value is
        // the parameter's either way. The `$` behind the word is what
        // makes these readable, so `graph` and `table` stay names.
        if let Some(param) = self.reference_parameter(&name) {
            return Ok(Expr::Param(param));
        }
        // The list value constructor of ISO 20.17, which names the
        // type it is building before it lists what goes in it. LIST
        // and ARRAY are the same word twice, as they are in the type
        // grammar, and the name is what the standard calls optional:
        // `LIST [1, 2]` and `[1, 2]` are the same list. It is here
        // with PATH because a bracket after a name is the only thing
        // that tells either of them from a variable read.
        if (name.eq_ignore_ascii_case("list") || name.eq_ignore_ascii_case("array"))
            && self.at(&TokenKind::LBracket)
        {
            return Ok(Expr::List(self.parse_bracketed_items()?));
        }
        // The record constructor of ISO 20.18, whose name is optional
        // in the same way: `RECORD {a: 1}` and `{a: 1}` are the same
        // record. A brace after a name begins nothing else, so
        // `record` stays free to be a variable everywhere else.
        if name.eq_ignore_ascii_case("record") && self.at(&TokenKind::LBrace) {
            return Ok(Expr::Map(self.parse_property_map()?));
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
        } else if name.eq_ignore_ascii_case("NORMALIZE") {
            // ISO 20.24, and here for the reason CAST is: the second
            // argument names one of the four normal forms, which is a
            // word and not a value, and a function would have received
            // a variable called NFC.
            self.parse_normalize()
        } else if name.eq_ignore_ascii_case("TRIM") {
            // ISO 20.24, and here for the reason NORMALIZE is: the
            // explicit form names an end of the string with a word and
            // separates its two operands with FROM, neither of which a
            // call can say. The one argument form comes through here
            // too and comes out as the call it is.
            self.parse_trim()
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
        let name = self.expect_variable("a variable name after YIELD")?;
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
        // ISO 21.2 writes a duration two ways, `DURATION 'P1D'` and the
        // SQL spelling, and the SQL one is a grammar of its own rather
        // than another string to read.
        if name.eq_ignore_ascii_case("INTERVAL") {
            return self.sql_interval_literal();
        }
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

    /// `INTERVAL '3 04:05:06' DAY TO SECOND`, the word already read.
    ///
    /// The qualifier is required, because it is what says how to read
    /// the string: `'1-2'` is a year and two months under `YEAR TO
    /// MONTH` and is not a duration at all under `DAY`. So the ISO
    /// 8601 spelling is `DURATION 'P1Y2M'` and never `INTERVAL
    /// 'P1Y2M'`, which is the standard's own division and is worth
    /// keeping: two spellings of one thing, one of them not GQL, is
    /// how a client learns a habit that travels nowhere.
    ///
    /// A sign may stand in front of the string and inside it, and both
    /// negate, so `INTERVAL -'-1' DAY` is one day forward.
    ///
    /// `None` means this was not an interval literal and `INTERVAL` is
    /// an ordinary name here.
    fn sql_interval_literal(&mut self) -> Result<Option<Expr>> {
        let signed = self.at(&TokenKind::Minus) || self.at(&TokenKind::Plus);
        let negative = self.at(&TokenKind::Minus);
        let at = self.pos + usize::from(signed);
        let Some(Token {
            kind: TokenKind::Str(text),
            ..
        }) = self.tokens.get(at)
        else {
            return Ok(None);
        };
        // A sign counts as part of the literal only when the whole
        // literal stands behind it, because `interval - '1'` is a
        // subtraction and reading its minus sign as this one would
        // refuse a statement that means something. With no sign there
        // is nothing to protect: no expression puts a string straight
        // after a name.
        if signed
            && !matches!(
                self.tokens.get(at + 1),
                Some(Token { kind: TokenKind::Ident(word), .. })
                    if IntervalField::spelled(word).is_some()
            )
        {
            return Ok(None);
        }
        let text = text.clone();
        self.pos = at + 1;
        let qualifier = self.interval_qualifier()?;
        let value = Temporal::parse_sql_interval(&text, &qualifier).ok_or_else(|| {
            ZuError::gql(
                codes::C22G0H,
                format!("'{text}' is not a {qualifier} interval anyone can read"),
            )
        })?;
        let value = match (negative, value) {
            (true, Temporal::Duration(kind, count)) => Temporal::Duration(kind, -count),
            (_, value) => value,
        };
        Ok(Some(Expr::Literal(Literal::Temporal(value))))
    }

    /// The `DAY`, `YEAR TO MONTH` or `SECOND(3)` behind an interval
    /// string.
    ///
    /// The two fields have to be the same kind of duration and the
    /// second has to be smaller than the first, which is one rule and
    /// not two: `MONTH TO DAY` fails both and there is no duration it
    /// could mean, since months and nanoseconds do not mix.
    fn interval_qualifier(&mut self) -> Result<IntervalQualifier> {
        let start = self.interval_field()?;
        let mut leading = None;
        let mut fraction = None;
        // The precision after the first field is the digits its own
        // value may have, and after `SECOND` a second number may
        // follow, which is the digits after the point.
        if self.at(&TokenKind::LParen) {
            let (first, second) = self.interval_precision()?;
            leading = Some(first);
            match (start, second) {
                (IntervalField::Second, second) => fraction = second,
                (_, Some(_)) => {
                    return Err(self.error("one precision, since only SECOND takes two"));
                }
                (_, None) => {}
            }
        }
        let mut end = start;
        if self.at_kw("TO") {
            let to = self.pos;
            self.pos += 1;
            end = self.interval_field()?;
            if self.at(&TokenKind::LParen) {
                if end != IntervalField::Second {
                    return Err(self.error("a comma or the end of the qualifier"));
                }
                let (first, second) = self.interval_precision()?;
                if second.is_some() {
                    return Err(self.error("one precision, since a fraction is one number"));
                }
                fraction = Some(first);
            }
            if start.kind() != end.kind() || start.rank() >= end.rank() {
                return Err(ZuError::gql_in(
                    codes::C42001,
                    self.source,
                    self.tokens[to].start,
                    format_args!(
                        "{} is not a field {} runs to, since a qualifier names one run of fields and a year is not a day",
                        end.word(),
                        start.word()
                    ),
                ));
            }
        }
        Ok(IntervalQualifier {
            start,
            end,
            leading,
            fraction,
        })
    }

    /// One of the six words a qualifier is written with.
    fn interval_field(&mut self) -> Result<IntervalField> {
        let field = match self.peek() {
            Some(Token {
                kind: TokenKind::Ident(word),
                ..
            }) => IntervalField::spelled(word),
            _ => None,
        };
        match field {
            Some(field) => {
                self.pos += 1;
                Ok(field)
            }
            None => Err(self.error("YEAR, MONTH, DAY, HOUR, MINUTE or SECOND")),
        }
    }

    /// `(3)` or `(3, 6)`, the parenthesis unconsumed.
    fn interval_precision(&mut self) -> Result<(u32, Option<u32>)> {
        self.expect(&TokenKind::LParen)?;
        let first = self.interval_digits(1)?;
        let second = match self.eat(&TokenKind::Comma) {
            true => Some(self.interval_digits(0)?),
            false => None,
        };
        self.expect(&TokenKind::RParen)?;
        Ok((first, second))
    }

    /// A digit count in a precision. The fraction may be none, since
    /// seconds to no decimal places is a thing to ask for, and the
    /// leading field may not, since a field of no digits holds no
    /// value at all.
    fn interval_digits(&mut self, least: u64) -> Result<u32> {
        match self.peek() {
            Some(Token {
                kind: TokenKind::Int(count),
                ..
            }) if (least..=9).contains(count) => {
                let count = *count as u32;
                self.pos += 1;
                Ok(count)
            }
            _ => Err(self.error(&format!("a digit count from {least} to 9"))),
        }
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

    /// `CASE`, ISO 20.7 and GE01, the word already read.
    ///
    /// Both forms are read here, and which one this is is settled by
    /// what follows `CASE`: a `WHEN` means the searched form, and
    /// anything else is the value the simple form compares each branch
    /// with.
    ///
    /// A branch of the simple form holds a `<when operand list>`, so it
    /// may name several operands and any one of them matching selects
    /// the result, and an operand may be the second half of a predicate
    /// rather than a value, `WHEN > 1` and `WHEN IS NULL` being two of
    /// the eight ISO lists. Both of those are the searched form written
    /// short: a list is the disjunction of the comparisons it stands
    /// for, and a predicate half is the predicate with the case operand
    /// as its left side. So a branch written either way is folded into
    /// the condition it abbreviates, and the whole expression becomes
    /// the searched form, since a subject compared with nothing is not
    /// a subject.
    ///
    /// The plain simple form is left as it stands, which is the point
    /// of folding only where something was abbreviated: `CASE a.kind
    /// WHEN 'x' THEN 1 WHEN 'y' THEN 2 END` keeps its one subject and
    /// reads `a.kind` once, where the fold would write it out per
    /// branch.
    fn parse_case(&mut self) -> Result<Expr> {
        let subject = match self.at_kw("WHEN") {
            true => None,
            false => Some(Box::new(self.parse_expr()?)),
        };
        let mut branches = Vec::new();
        let mut abbreviated = false;
        while self.eat_kw("WHEN") {
            let when = match &subject {
                None => vec![Operand::Condition(self.parse_expr()?)],
                Some(subject) => {
                    let mut operands = vec![self.parse_when_operand(subject)?];
                    while self.eat(&TokenKind::Comma) {
                        operands.push(self.parse_when_operand(subject)?);
                    }
                    abbreviated |= operands.len() > 1
                        || operands
                            .iter()
                            .any(|operand| matches!(operand, Operand::Condition(_)));
                    operands
                }
            };
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
        // The subject is written into the branches only where something
        // was abbreviated, and then it is written into all of them,
        // since a case is one form or the other and not a branch of
        // each.
        let written = match abbreviated {
            true => subject.as_deref(),
            false => None,
        };
        let branches = branches
            .into_iter()
            .map(|(when, then)| (fold_when(when, written), then))
            .collect();
        Ok(Expr::Case {
            subject: match abbreviated {
                true => None,
                false => subject,
            },
            branches,
            otherwise,
        })
    }

    /// One `<when operand>` of the simple form, `subject` being the
    /// case operand it is read against.
    ///
    /// A comparison operator or an `IS` standing where a value belongs
    /// is the second half of a predicate, which the standard allows
    /// exactly here: the case operand is its left side, so the operand
    /// is that predicate with the subject written into it. `IS` covers
    /// six of ISO's eight alternatives on its own, the null test, the
    /// value type test, the normalized test and the three pattern
    /// predicates all being written behind it, so the two branches here
    /// are the whole list.
    fn parse_when_operand(&mut self, subject: &Expr) -> Result<Operand> {
        let op = match self.peek().map(|t| &t.kind) {
            Some(TokenKind::Eq) => BinaryOp::Eq,
            Some(TokenKind::Ne) => BinaryOp::Ne,
            Some(TokenKind::Lt) => BinaryOp::Lt,
            Some(TokenKind::Le) => BinaryOp::Le,
            Some(TokenKind::Gt) => BinaryOp::Gt,
            Some(TokenKind::Ge) => BinaryOp::Ge,
            _ => {
                if self.eat_kw("IS") {
                    let predicate = self.parse_is_tail(subject.clone())?;
                    return Ok(Operand::Condition(predicate));
                }
                return Ok(Operand::Value(self.parse_expr()?));
            }
        };
        self.pos += 1;
        let right = self.parse_concat()?;
        Ok(Operand::Condition(binary(op, subject.clone(), right)))
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
            // GV56 and GV57 in their open spelling. ANY is optional in
            // front of both, so the word is read here as well as below
            // rather than the prefix being a type of its own.
            if let Some(ty) = self.parse_reference_type()? {
                return Ok(ty);
            }
            self.eat_kw("VALUE");
            return Ok(LogicalType::Any);
        }
        if let Some(ty) = self.parse_reference_type()? {
            return Ok(ty);
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
        //
        // ISO writes it in square brackets, `LIST<INT>[2]`, which is
        // the only length in the grammar not written in parentheses.
        // Both are read, since a query that spells it the way every
        // other length is spelled meant the same thing.
        let max = if self.eat(&TokenKind::LBracket) {
            let n = self.parse_type_argument()?;
            self.expect(&TokenKind::RBracket)?;
            Some(n)
        } else if self.eat(&TokenKind::LParen) {
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

    /// A node or edge reference value type, GV56 and GV57, or `None`
    /// where the next word is neither synonym.
    ///
    /// Open and closed are one production here because they differ
    /// only in what follows the synonym: `NODE` on its own admits any
    /// node, and `NODE :Person` admits the ones wearing that label.
    /// The two are the same word read one token further, so a caller
    /// that had to choose between them before reading would have to
    /// look ahead anyway.
    ///
    /// The label set is one name rather than a label expression. A
    /// reference type in zu carries a name, and widening it to an
    /// expression is worth doing when a cast to a disjunction is a
    /// thing somebody writes; refusing the rest here says so plainly
    /// rather than accepting a conjunction and checking one half of it.
    fn parse_reference_type(&mut self) -> Result<Option<LogicalType>> {
        let node = self.at_kw("NODE") || self.at_kw("VERTEX");
        if !node && !(self.at_kw("EDGE") || self.at_kw("RELATIONSHIP")) {
            return Ok(None);
        }
        self.pos += 1;
        self.eat_kw("TYPE");
        let mut label = None;
        // `NODE (:Person)` is the pattern spelling of the same thing,
        // and the parenthesis is what tells it from the phrase.
        let parenthesised = self.eat(&TokenKind::LParen);
        if self.eat(&TokenKind::Colon) {
            label = Some(self.expect_name("a label in a reference type")?);
        }
        if parenthesised {
            self.expect(&TokenKind::RParen)?;
        }
        Ok(Some(if node {
            LogicalType::Node(label)
        } else {
            LogicalType::Edge(label)
        }))
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
                let name = self.expect_field_name("a field name")?;
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
        if !self.at_double_colon() {
            return Err(self.error("'::'"));
        }
        self.pos += 2;
        Ok(())
    }

    /// Whether a `::` stands here, without taking it.
    fn at_double_colon(&self) -> bool {
        match (self.tokens.get(self.pos), self.tokens.get(self.pos + 1)) {
            (Some(first), Some(second)) => {
                first.kind == TokenKind::Colon
                    && second.kind == TokenKind::Colon
                    && first.start + 1 == second.start
            }
            _ => false,
        }
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
        let key = self.expect_field_name("a property name")?;
        self.expect(&TokenKind::RParen)?;
        Ok(Expr::PropertyExists { expr, key })
    }

    /// `NORMALIZE(s)` or `NORMALIZE(s, NFKC)`, the parenthesis
    /// unconsumed and NORMALIZE already read.
    fn parse_normalize(&mut self) -> Result<Expr> {
        self.expect(&TokenKind::LParen)?;
        let expr = Box::new(self.parse_expr()?);
        let form = match self.eat(&TokenKind::Comma) {
            true => self.parse_normal_form()?,
            false => NormalForm::Nfc,
        };
        self.expect(&TokenKind::RParen)?;
        Ok(Expr::Normalize { expr, form })
    }

    /// `TRIM(s)`, `TRIM(c FROM s)`, `TRIM(LEADING c FROM s)` or
    /// `TRIM(list, n)`, the parenthesis unconsumed and TRIM already
    /// read.
    ///
    /// Four spellings and, in the grammar, three rules:
    /// `<single-character trim function>` over a string,
    /// `<byte string trim function>` over octets, and
    /// `<trim list function>` over a list and a count. The first two
    /// share a shape and this reads them as one; the third is the comma
    /// form and is told apart by the comma alone, because a FROM never
    /// follows one and a count is written where nothing else may be.
    ///
    /// Inside the first two, the trim specification and the trim
    /// character are both optional, so what stands after the
    /// parenthesis may be an end of the string, the character to take
    /// off it, or the string itself, and only what follows says which.
    fn parse_trim(&mut self) -> Result<Expr> {
        self.expect(&TokenKind::LParen)?;
        let side = self.eat_trim_side();
        // With an end named, what follows is the character or the FROM,
        // and the string is behind the FROM. Without one, the first
        // expression is the character if a FROM follows it and the
        // string if nothing does.
        let (chars, source) = if side.is_some() {
            let chars = match self.at_kw("FROM") {
                true => None,
                false => Some(Box::new(self.parse_expr()?)),
            };
            self.expect_kw("FROM")?;
            (chars, self.parse_expr()?)
        } else {
            let first = self.parse_expr()?;
            if self.eat(&TokenKind::Comma) {
                // The list form. The count goes where the character to
                // take off goes, because both are the one thing the
                // trim is given besides what it is trimming, and the
                // kernel reads the first argument to know which it has.
                let count = self.parse_expr()?;
                (Some(Box::new(count)), first)
            } else {
                match self.eat_kw("FROM") {
                    true => (Some(Box::new(first)), self.parse_expr()?),
                    false => (None, first),
                }
            }
        };
        self.expect(&TokenKind::RParen)?;
        Ok(Expr::Trim {
            side: side.unwrap_or(TrimSide::Both),
            chars,
            source: Box::new(source),
        })
    }

    /// The trim specification, if one is written here.
    ///
    /// A word is only taken as one when something other than the end of
    /// the call follows it, so `TRIM(leading)` reads the variable a
    /// query wrote and `TRIM(LEADING FROM s)` reads the specification.
    /// The three words are reserved in ISO and are ordinary identifiers
    /// to this lexer, which is the arrangement every keyword here has.
    fn eat_trim_side(&mut self) -> Option<TrimSide> {
        let side = if self.at_kw("LEADING") {
            TrimSide::Leading
        } else if self.at_kw("TRAILING") {
            TrimSide::Trailing
        } else if self.at_kw("BOTH") {
            TrimSide::Both
        } else {
            return None;
        };
        if matches!(
            self.tokens.get(self.pos + 1).map(|t| &t.kind),
            None | Some(TokenKind::RParen) | Some(TokenKind::Comma)
        ) {
            return None;
        }
        self.pos += 1;
        Some(side)
    }

    /// One of the four words ISO 19.7 names a normal form with. They are
    /// reserved words, so nothing else can be standing here, and a word
    /// that is not one of the four is named in the error rather than
    /// read as a variable.
    fn parse_normal_form(&mut self) -> Result<NormalForm> {
        // The error is raised before the word is taken, so it points at
        // the word that is wrong rather than at whatever follows it.
        let expected = "a normal form: NFC, NFD, NFKC or NFKD";
        if !self.at_normal_form() {
            return Err(self.error(expected));
        }
        let word = self.expect_name(expected)?;
        NormalForm::from_name(&word).ok_or_else(|| self.error(expected))
    }

    /// Whether a normal form is written next. Only the four words are,
    /// so `x IS NORMALIZED AND y` reads the AND as the AND it is.
    fn at_normal_form(&self) -> bool {
        self.peek()
            .and_then(|t| match &t.kind {
                TokenKind::Ident(word) => NormalForm::from_name(word),
                _ => None,
            })
            .is_some()
    }

    fn parse_call(&mut self, name: String) -> Result<Expr> {
        self.expect(&TokenKind::LParen)?;
        // The set quantifier of a general set function, which the
        // standard spells `DISTINCT | ALL`. `ALL` is what leaving it
        // out means, so it is read and dropped rather than carried:
        // the two spellings are one query and should be one plan.
        let distinct = self.eat_kw("DISTINCT");
        if !distinct {
            self.eat_kw("ALL");
        }
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

    /// One of the temporal value functions of ISO 20.27 and 20.29, with
    /// the word read and its brackets, if it has any, standing.
    ///
    /// Empty brackets read the clock, which is what the word alone does
    /// and is why `LOCAL_TIME` and `LOCAL_TIME()` are one function. A
    /// duration has no such form, there being a date now and a time now
    /// and no length of time now, so its brackets have to hold a value.
    fn parse_temporal_fn(&mut self, func: TemporalFn) -> Result<Expr> {
        let mut arg = None;
        if self.eat(&TokenKind::LParen) {
            if !self.at(&TokenKind::RParen) {
                arg = Some(Box::new(self.parse_expr()?));
            } else if !func.reads_the_clock() {
                return Err(self.error(&format!(
                    "a string for {}(), since there is no length of time now",
                    func.word().to_uppercase()
                )));
            }
            self.expect(&TokenKind::RParen)?;
        }
        Ok(Expr::Temporal { func, arg })
    }

    /// `DURATION_BETWEEN(a, b) [YEAR TO MONTH | DAY TO SECOND]`, ISO
    /// 20.28, with the name read and the bracket standing.
    ///
    /// The arguments are read the way a call's are and counted where a
    /// call's are, on the row, so `DURATION_BETWEEN(a)` is refused with
    /// the same words every other short call is refused with.
    fn parse_duration_between(&mut self) -> Result<Expr> {
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
        Ok(Expr::DurationBetween {
            args,
            kind: self.temporal_duration_qualifier()?,
        })
    }

    /// The `YEAR TO MONTH` or `DAY TO SECOND` behind a datetime
    /// subtraction, or nothing where none is written.
    ///
    /// Only those two runs are a temporal duration qualifier, which is
    /// narrower than an interval qualifier: the answer is a whole
    /// duration of one kind or the other and never a run of fields
    /// inside one. The words are read only when a `TO` and a second
    /// field stand behind the first, so nothing that is not a
    /// qualifier is eaten and `MONTH TO DAY` still reaches a refusal
    /// that names it.
    fn temporal_duration_qualifier(&mut self) -> Result<Option<DurationKind>> {
        let field = |at: usize| match self.tokens.get(at) {
            Some(Token {
                kind: TokenKind::Ident(word),
                ..
            }) => IntervalField::spelled(word),
            _ => None,
        };
        let (Some(start), Some(end)) = (field(self.pos), field(self.pos + 2)) else {
            return Ok(None);
        };
        if !matches!(self.tokens.get(self.pos + 1), Some(Token { kind: TokenKind::Ident(word), .. }) if word.eq_ignore_ascii_case("TO"))
        {
            return Ok(None);
        }
        let at = self.pos;
        self.pos += 3;
        match (start, end) {
            (IntervalField::Year, IntervalField::Month) => Ok(Some(DurationKind::YearMonth)),
            (IntervalField::Day, IntervalField::Second) => Ok(Some(DurationKind::DayTime)),
            _ => Err(ZuError::gql_in(
                codes::C42001,
                self.source,
                self.tokens[at].start,
                format_args!(
                    "{} TO {} is not a duration qualifier, which is YEAR TO MONTH or DAY TO SECOND and nothing else",
                    start.word(),
                    end.word()
                ),
            )),
        }
    }
}

/// The datetime value function a word names, or nothing where the word
/// names none and is an ordinary identifier. The five words are the
/// names their rows carry, so there is one list of them and it is the
/// one the plan prints from.
fn temporal_word(name: &str) -> Option<TemporalFn> {
    [
        TemporalFn::CurrentDate,
        TemporalFn::CurrentTime,
        TemporalFn::CurrentTimestamp,
        TemporalFn::LocalTime,
        TemporalFn::LocalTimestamp,
        TemporalFn::Date,
        TemporalFn::ZonedTime,
        TemporalFn::ZonedDatetime,
        TemporalFn::LocalDatetime,
        TemporalFn::Duration,
    ]
    .into_iter()
    .find(|func| func.word().eq_ignore_ascii_case(name))
}

fn binary(op: BinaryOp, lhs: Expr, rhs: Expr) -> Expr {
    Expr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

/// One `<when operand>` of a case expression (ISO 20.7), as read.
///
/// A value is one the case operand is compared with, and a condition is
/// one that already holds the case operand, being a predicate written
/// without its left side. The searched form writes conditions and
/// nothing else, and the simple form may write either.
enum Operand {
    Value(Expr),
    Condition(Expr),
}

/// The one condition a branch comes to.
///
/// `subject` is the case operand where it has to be written into the
/// branch, and `None` where the branch is a condition already, which is
/// the searched form and the simple form that abbreviated nothing. Any
/// operand matching selects the result, so the operands of one branch
/// meet under `OR`, and null behaves the way it does in every other
/// disjunction: a branch that is neither true nor false is one the walk
/// passes over.
fn fold_when(operands: Vec<Operand>, subject: Option<&Expr>) -> Expr {
    let mut folded: Option<Expr> = None;
    for operand in operands {
        let condition = match (operand, subject) {
            (Operand::Value(value), None) => value,
            (Operand::Value(value), Some(subject)) => binary(BinaryOp::Eq, subject.clone(), value),
            (Operand::Condition(condition), _) => condition,
        };
        folded = Some(match folded {
            None => condition,
            Some(left) => binary(BinaryOp::Or, left, condition),
        });
    }
    folded.expect("a WHEN holds at least one operand")
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

    fn session_stmt(source: &str) -> SessionStmt {
        match parse_statement(source).expect("parse") {
            Statement::Session(stmt) => stmt,
            other => panic!("parsed as {other:?}"),
        }
    }

    fn session_err(source: &str) -> String {
        parse_statement(source)
            .expect_err("should fail")
            .to_string()
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
               (:Person)-[:KNOWS => :Nearby]->(:Org),
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

    /// The three parameter kinds, which are the three a binding
    /// variable has and are written the same way behind the name (GS01,
    /// GS02, GS03).
    #[test]
    fn a_session_parameter_is_set_in_each_of_its_three_kinds() {
        let kinds = [
            ("SESSION SET VALUE $v = 1", BindingKind::Value, "v"),
            (
                "SESSION SET BINDING TABLE $t = { MATCH (p) RETURN p AS p }",
                BindingKind::Table,
                "t",
            ),
            (
                "SESSION SET PROPERTY GRAPH $g = CURRENT_PROPERTY_GRAPH",
                BindingKind::Graph,
                "g",
            ),
        ];
        for (source, kind, name) in kinds {
            let SessionStmt::SetParameter { def, if_not_exists } = session_stmt(source) else {
                panic!("{source} is not a parameter");
            };
            assert_eq!(def.kind, kind, "{source}");
            assert_eq!(def.name, name, "{source}");
            assert!(!if_not_exists, "{source}");
        }
        // The long spellings are the short ones. `TABLE` alone is a
        // binding table and `GRAPH` alone is a property graph, which is
        // how a binding variable definition reads them too.
        assert_eq!(
            session_stmt("SESSION SET TABLE $t = $u"),
            session_stmt("SESSION SET BINDING TABLE $t = $u")
        );
        assert_eq!(
            session_stmt("SESSION SET GRAPH $g = HOME_GRAPH"),
            session_stmt("SESSION SET PROPERTY GRAPH $g = HOME_PROPERTY_GRAPH")
        );
    }

    /// A definition may be written with a type and may be initialized
    /// from a query in braces, which is the whole of GS10 and GS11: the
    /// same two forms a binding variable takes.
    #[test]
    fn a_session_parameter_takes_a_type_and_a_query() {
        let SessionStmt::SetParameter { def, .. } =
            session_stmt("SESSION SET VALUE $n :: INTEGER = { MATCH (p) RETURN count(*) AS n }")
        else {
            panic!("not a parameter");
        };
        assert_eq!(
            def.ty,
            Some(LogicalType::Nullable(Box::new(
                value_type::spelled("INTEGER", &[]).expect("a type")
            )))
        );
        assert!(matches!(def.init, BindingInit::Query(_)));
        let SessionStmt::SetParameter { def, if_not_exists } =
            session_stmt("SESSION SET VALUE IF NOT EXISTS $n = 1")
        else {
            panic!("not a parameter");
        };
        assert!(if_not_exists);
        assert_eq!(def.name, "n");
    }

    /// `SESSION SET PROPERTY GRAPH` is written two ways and the dollar
    /// is the whole difference: with one it binds a parameter, without
    /// one it moves the graph the session works in.
    #[test]
    fn the_dollar_tells_a_graph_parameter_from_the_session_graph() {
        assert_eq!(
            session_stmt("SESSION SET PROPERTY GRAPH social"),
            SessionStmt::SetGraph(GraphRef::Named(GraphName {
                schema: None,
                name: "social".to_string(),
            }))
        );
        assert_eq!(
            session_stmt("SESSION SET GRAPH HOME_PROPERTY_GRAPH"),
            SessionStmt::SetGraph(GraphRef::Home)
        );
        let SessionStmt::SetParameter { def, .. } =
            session_stmt("SESSION SET PROPERTY GRAPH $g = social")
        else {
            panic!("not a parameter");
        };
        assert_eq!(def.kind, BindingKind::Graph);
    }

    #[test]
    fn a_session_takes_a_schema_and_a_zone() {
        assert_eq!(
            session_stmt("SESSION SET SCHEMA /app"),
            SessionStmt::SetSchema(SchemaRef::Path("/app".to_string()))
        );
        assert_eq!(
            session_stmt("SESSION SET SCHEMA /"),
            SessionStmt::SetSchema(SchemaRef::Path("/".to_string()))
        );
        // GS15. Every spelling of a displacement a zoned literal takes,
        // and nought written either way of the sign is nought.
        for (source, minutes) in [
            ("SESSION SET TIME ZONE '+07:00'", 420),
            ("SESSION SET TIME ZONE '-05:30'", -330),
            ("SESSION SET TIME ZONE '+0700'", 420),
            ("SESSION SET TIME ZONE '+07'", 420),
            ("SESSION SET TIME ZONE 'Z'", 0),
            ("SESSION SET TIME ZONE '-00:00'", 0),
        ] {
            assert_eq!(session_stmt(source), SessionStmt::SetTimeZone(minutes));
        }
    }

    /// A zone name is not a displacement, and the refusal says so
    /// rather than letting the session hold a rule the zone database
    /// can change (`02 §3.4`).
    #[test]
    fn a_zone_name_is_not_a_displacement() {
        let err = session_err("SESSION SET TIME ZONE 'Europe/Dublin'");
        assert!(err.contains("no time zone displacement"), "{err}");
        assert!(session_err("SESSION SET TIME ZONE '+19:00'").contains("displacement"));
        assert!(session_err("SESSION SET TIME ZONE 7").contains("in quotes"));
    }

    #[test]
    fn every_reset_is_read_and_a_bare_one_is_the_widest() {
        let resets = [
            ("SESSION RESET", SessionReset::Characteristics),
            ("SESSION RESET;", SessionReset::Characteristics),
            (
                "SESSION RESET ALL CHARACTERISTICS",
                SessionReset::Characteristics,
            ),
            (
                "SESSION RESET CHARACTERISTICS",
                SessionReset::Characteristics,
            ),
            ("SESSION RESET ALL PARAMETERS", SessionReset::Parameters),
            ("SESSION RESET PARAMETERS", SessionReset::Parameters),
            ("SESSION RESET SCHEMA", SessionReset::Schema),
            ("SESSION RESET GRAPH", SessionReset::Graph),
            ("SESSION RESET PROPERTY GRAPH", SessionReset::Graph),
            ("SESSION RESET TIME ZONE", SessionReset::TimeZone),
            (
                "SESSION RESET PARAMETER $p",
                SessionReset::Parameter("p".to_string()),
            ),
        ];
        for (source, reset) in resets {
            assert_eq!(session_stmt(source), SessionStmt::Reset(reset), "{source}");
        }
        assert!(session_err("SESSION RESET ALL SCHEMA").contains("SESSION RESET ALL"));
        assert!(session_err("SESSION RESET PARAMETER p").contains("with a dollar"));
        assert!(session_err("SESSION RESET WHAT").contains("after SESSION RESET"));
    }

    /// A session statement is whole on its own, the way a transaction
    /// statement is: it has no binding table for anything behind it to
    /// read, so a `NEXT` after one is a statement with nowhere to go.
    #[test]
    fn nothing_follows_a_session_statement() {
        let err = session_err("SESSION RESET SCHEMA NEXT MATCH (p) RETURN p");
        assert!(
            err.contains("nothing may follow a session statement"),
            "{err}"
        );
        assert!(session_err("SESSION SET VALUE 1 = 1").contains("with a dollar"));
        assert!(session_err("SESSION SET WHAT $p = 1").contains("after SESSION SET"));
        assert!(session_err("SESSION").contains("expected"));
    }

    /// A statement GQL defines and the v0 core does not parse should be
    /// turned away by name. Being told the parser expected MATCH sends a
    /// reader looking for a typo in a statement they spelled correctly,
    /// which is the wrong place to look and the wrong thing to fix.
    #[test]
    fn a_statement_we_do_not_parse_yet_is_refused_by_name() {
        let err = parse_err("CREATE (n) RETURN n");
        assert!(
            err.contains("CREATE is not implemented yet"),
            "refused with {err:?}, which does not name CREATE"
        );
    }

    /// `MERGE` reads one pattern and then whichever of the two `ON`
    /// blocks were written, in either order.
    #[test]
    fn merge_reads_one_pattern_and_two_blocks() {
        let q = parse("MERGE (p:person {id: 7}) ON MATCH SET p.seen = 1 ON CREATE SET p.made = 2")
            .expect("parse");
        let Clause::Merge {
            pattern,
            on_create,
            on_match,
        } = &q.clauses()[0]
        else {
            panic!("parsed as {:?}", q.clauses()[0]);
        };
        assert!(pattern.steps.is_empty(), "one element and no steps");
        assert_eq!(on_create.len(), 1);
        assert_eq!(on_create[0].target, "p");
        assert_eq!(on_match.len(), 1);
        assert_eq!(on_match[0].target, "p");
        let plain = parse("MERGE (p:person)").expect("parse");
        let Clause::Merge {
            on_create,
            on_match,
            ..
        } = &plain.clauses()[0]
        else {
            panic!("parsed as {:?}", plain.clauses()[0]);
        };
        assert!(on_create.is_empty() && on_match.is_empty(), "neither block");
    }

    /// The shapes the word does not take: two patterns, which would
    /// leave unsaid which of them was found and which written, and
    /// either block written twice, which would leave unsaid what order
    /// the items run in.
    #[test]
    fn merge_takes_one_pattern_and_each_block_once() {
        assert!(
            parse_err("MERGE (a:person), (b:person)").contains("MERGE takes one pattern"),
            "{}",
            parse_err("MERGE (a:person), (b:person)")
        );
        for (source, kw) in [
            (
                "MERGE (p:person) ON CREATE SET p.a = 1 ON CREATE SET p.b = 2",
                "ON CREATE SET is written once",
            ),
            (
                "MERGE (p:person) ON MATCH SET p.a = 1 ON MATCH SET p.b = 2",
                "ON MATCH SET is written once",
            ),
        ] {
            let err = parse_err(source);
            assert!(err.contains(kw), "{source:?} was refused with {err:?}");
        }
    }

    /// `FINISH` is a result statement and the clause list ends with
    /// it, since what it does is end the statement rather than say
    /// what the answer looks like.
    #[test]
    fn finish_ends_a_statement_with_no_result() {
        let query = parsed("MATCH (p:Person) FINISH");
        let linear = linear_body(&query);
        assert_eq!(linear.statements.len(), 1);
        let simple = &linear.statements[0];
        assert!(simple.result.is_none(), "there is no projection");
        assert!(
            matches!(simple.clauses.last(), Some(Clause::Finish)),
            "the last clause is the FINISH, got {:?}",
            simple.clauses.last()
        );
    }

    /// Nothing may follow it and nothing may read from it, which is
    /// the difference between a statement that answers nothing and a
    /// statement that has not answered yet.
    #[test]
    fn nothing_follows_a_finish() {
        assert!(
            parse_err("MATCH (p) FINISH RETURN 1 AS one").contains("nothing may follow FINISH")
        );
        assert!(
            parse_err("MATCH (p) FINISH NEXT RETURN 1 AS one")
                .contains("FINISH is how a statement says it returns nothing")
        );
        assert!(
            parse_err("MATCH (p) FINISH UNION RETURN 1 AS one").contains("has to end with RETURN"),
            "a conjunction joins two result tables"
        );
    }

    /// ISO 14.9 makes the order by and page a statement of its own, so
    /// each of its three parts stands where a statement stands.
    #[test]
    fn the_order_by_and_page_is_a_statement() {
        for (source, keys, skip, limit) in [
            (
                "MATCH (p) ORDER BY p.age RETURN p.name AS name",
                1,
                false,
                false,
            ),
            (
                "MATCH (p) ORDER BY p.age, p.name RETURN p.name AS name",
                2,
                false,
                false,
            ),
            ("MATCH (p) OFFSET 1 RETURN p.name AS name", 0, true, false),
            (
                "MATCH (p) SKIP 1 LIMIT 2 RETURN p.name AS name",
                0,
                true,
                true,
            ),
            ("MATCH (p) LIMIT 2 RETURN p.name AS name", 0, false, true),
        ] {
            let query = parsed(source);
            let simple = &linear_body(&query).statements[0];
            let Some(Clause::Order {
                keys: k,
                skip: s,
                limit: l,
            }) = simple.clauses.get(1)
            else {
                panic!("{source:?} parsed as {:?}", simple.clauses);
            };
            assert_eq!(k.len(), keys, "{source:?}");
            assert_eq!(s.is_some(), skip, "{source:?}");
            assert_eq!(l.is_some(), limit, "{source:?}");
        }
    }

    /// The same words behind a `RETURN` belong to that projection, so
    /// a statement of them is only ever what is left over.
    #[test]
    fn the_page_behind_a_return_is_still_the_projections() {
        let query = parsed("MATCH (p) RETURN p.name AS name ORDER BY name LIMIT 2");
        let simple = &linear_body(&query).statements[0];
        assert_eq!(simple.clauses.len(), 1, "the MATCH, and no page clause");
        let projection = simple.result.as_ref().expect("a projection");
        assert_eq!(projection.order_by.len(), 1);
        assert!(projection.limit.is_some());
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

    fn block_parts(source: &str) -> Vec<String> {
        match parse_statement(source).expect("parse") {
            Statement::Block(parts) => parts,
            other => panic!("parsed as {other:?}"),
        }
    }

    /// GP18. A `NEXT` handing over to a catalog statement is where a
    /// linear query statement ends, so the chain is cut there and each
    /// side of the cut is a part of the block.
    #[test]
    fn a_next_onto_a_catalog_statement_cuts_the_statement_into_parts() {
        assert_eq!(
            block_parts(
                "CREATE GRAPH TYPE t { (:Person) } NEXT INSERT (p:Person) \
                 NEXT MATCH (q:Person) RETURN q AS q NEXT DROP GRAPH TYPE t"
            ),
            vec![
                "CREATE GRAPH TYPE t { (:Person) }",
                "INSERT (p:Person) NEXT MATCH (q:Person) RETURN q AS q",
                "DROP GRAPH TYPE t",
            ],
        );
        assert_eq!(
            block_parts("MATCH (p:Person) RETURN p AS p NEXT CREATE GRAPH g ANY;"),
            vec!["MATCH (p:Person) RETURN p AS p", "CREATE GRAPH g ANY"],
        );
    }

    /// The cut is only made where a catalog statement stands, so a
    /// chain of query statements is the one statement it always was.
    #[test]
    fn a_next_onto_a_query_stays_inside_the_one_statement() {
        let q = parsed("MATCH (p:Person) RETURN p AS p NEXT MATCH (p)-[:KNOWS]->(f) RETURN f AS f");
        assert_eq!(linear_body(&q).statements.len(), 2);
    }

    /// A block runs as one transaction, so a word that begins or ends
    /// one is not a part of it.
    #[test]
    fn a_transaction_word_is_not_a_part_of_a_block() {
        assert!(
            catalog_err("CREATE GRAPH g ANY NEXT COMMIT").contains("runs as one transaction"),
            "the refusal says why the word has no place there"
        );
    }

    /// GP18. A call body is a procedure body too, so a catalog
    /// statement may open one, and where the call is the front of the
    /// statement it is taken out and run in front of what is left.
    #[test]
    fn a_catalog_statement_at_the_front_of_a_call_body_is_lifted_out_of_it() {
        assert_eq!(
            block_parts(
                "CALL {\n  CREATE PROPERTY GRAPH mixed ANY\n  NEXT\n  \
                 MATCH (p:Person)\n  RETURN COUNT(*) AS n\n}\nRETURN n AS n"
            ),
            vec![
                "CREATE PROPERTY GRAPH mixed ANY",
                "CALL {\n  MATCH (p:Person)\n  RETURN COUNT(*) AS n\n}\nRETURN n AS n",
            ],
        );
        assert_eq!(
            block_parts(
                "CALL () { CREATE GRAPH a ANY NEXT DROP GRAPH b NEXT MATCH (p) RETURN p AS p } \
                 RETURN p AS p NEXT DROP GRAPH a"
            ),
            vec![
                "CREATE GRAPH a ANY",
                "DROP GRAPH b",
                "CALL () { MATCH (p) RETURN p AS p } RETURN p AS p",
                "DROP GRAPH a",
            ],
        );
    }

    /// A catalog statement is refused where taking it out of the body
    /// would change what it means: in a call the statement does not
    /// begin with, which runs for every row that reaches it, and behind
    /// the data statements of a body rather than in front of them.
    #[test]
    fn a_catalog_statement_is_not_written_deeper_inside_a_call_body() {
        for source in [
            "MATCH (p:Person) CALL (p) { CREATE GRAPH g ANY NEXT INSERT (:Person) } RETURN p AS p",
            "CALL { INSERT (:Person) NEXT DROP GRAPH g }",
            "MATCH (p:Person) CALL (p) { INSERT (:Person) NEXT DROP GRAPH g } RETURN p AS p",
        ] {
            assert!(
                catalog_err(source).contains("written beside the call"),
                "{source}"
            );
        }
    }

    /// A body answers rows and a catalog statement answers none, so a
    /// body that is only a catalog statement is refused.
    #[test]
    fn a_call_body_that_is_only_a_catalog_statement_is_refused() {
        assert!(
            catalog_err("CALL () { CREATE GRAPH g ANY }").contains("hands over to a statement"),
            "the refusal says what is missing behind it"
        );
    }

    /// GP06. ISO writes the separator between a binding variable and
    /// its type two ways and lets the type be written with neither, so
    /// all three spellings say the same thing here.
    #[test]
    fn a_binding_variable_takes_its_type_however_the_separator_is_written() {
        for source in [
            "VALUE t :: INT = 3 MATCH (p:Person) RETURN t AS t",
            "VALUE t TYPED INT = 3 MATCH (p:Person) RETURN t AS t",
            "VALUE t INT = 3 MATCH (p:Person) RETURN t AS t",
        ] {
            let def = &parsed(source).bindings[0];
            assert_eq!(def.name, "t", "{source}");
            assert!(def.ty.is_some(), "{source}");
        }
        // The equals is what says the type is over, so a definition
        // written without one has no type rather than a missing one.
        assert!(
            parsed("VALUE t = 3 MATCH (p:Person) RETURN t AS t").bindings[0]
                .ty
                .is_none()
        );
    }

    /// GP16. The `AT` clause stands in front of the query, ahead of the
    /// graph clause and the definitions, and names a schema three ways.
    #[test]
    fn an_at_clause_names_the_schema_a_query_resolves_in() {
        for (source, want) in [
            (
                "AT CURRENT_SCHEMA MATCH (p) RETURN p AS p",
                SchemaRef::Current,
            ),
            ("AT HOME_SCHEMA MATCH (p) RETURN p AS p", SchemaRef::Home),
            (
                "AT /app MATCH (p) RETURN p AS p",
                SchemaRef::Path("/app".into()),
            ),
            // `SCHEMA` in front of the path is the long spelling and
            // says nothing the path does not.
            (
                "AT SCHEMA /app MATCH (p) RETURN p AS p",
                SchemaRef::Path("/app".into()),
            ),
        ] {
            assert_eq!(parsed(source).at_schema, Some(want), "{source}");
        }
        assert_eq!(parsed("MATCH (p) RETURN p AS p").at_schema, None);
    }

    /// The word opens the schema of a named procedure call as well, and
    /// the two never collide: this one is read where a query begins and
    /// that one behind the `CALL`.
    #[test]
    fn an_at_clause_and_the_schema_of_a_call_are_read_apart() {
        let q = parsed("AT /app CALL AT / pagerank() YIELD rank RETURN rank AS rank");
        assert_eq!(q.at_schema, Some(SchemaRef::Path("/app".into())));
        let Some(Clause::Call { at, .. }) = linear_body(&q).statements[0].clauses.first() else {
            panic!("the first clause is the call");
        };
        assert_eq!(at.as_deref(), Some("/"));
    }

    /// GP16 again. A call the statement begins with is taken out of the
    /// call and put in the chain around it, so an `AT` written on the
    /// body is the schema the query it lands in resolves in.
    #[test]
    fn an_at_clause_on_a_hoisted_call_body_becomes_the_querys_own() {
        let q = parsed("CALL { AT /app MATCH (p:Person) RETURN COUNT(*) AS n } RETURN n AS n");
        assert_eq!(q.at_schema, Some(SchemaRef::Path("/app".into())));
        assert_eq!(linear_body(&q).statements.len(), 2);
    }

    /// GP12. The graph clause belongs to the statement rather than to
    /// the head, so ISO lets it stand behind the definitions as well as
    /// in front of them, and a clause naming a graph a definition above
    /// it defined can only be written there.
    #[test]
    fn a_use_clause_stands_on_either_side_of_the_definitions() {
        let front = parsed("USE g MATCH (p:Person) RETURN p AS p");
        let behind =
            parsed("GRAPH g = CURRENT_PROPERTY_GRAPH USE g MATCH (p:Person) RETURN p AS p");
        assert!(matches!(front.use_graph, Some(GraphRef::Named(_))));
        assert_eq!(front.use_graph, behind.use_graph);
        assert_eq!(behind.bindings.len(), 1);
        // A statement runs against one graph, so it names one once.
        let err = parse("USE h GRAPH g = CURRENT_PROPERTY_GRAPH USE g MATCH (p) RETURN p AS p")
            .unwrap_err()
            .to_string();
        assert!(err.contains("names one once"), "{err}");
    }

    /// GP11 through GP13. A body naming a graph of its own comes out of
    /// the call the way a schema clause does, and its definitions come
    /// with it, the clause being allowed to read one.
    #[test]
    fn a_use_clause_on_a_hoisted_call_body_becomes_the_querys_own() {
        let q = parsed(
            "CALL { PROPERTY GRAPH g = CURRENT_PROPERTY_GRAPH USE g \
             MATCH (p:Person) RETURN COUNT(*) AS n } RETURN n AS n",
        );
        assert_eq!(
            q.use_graph,
            Some(GraphRef::Named(GraphName {
                schema: None,
                name: "g".to_string(),
            }))
        );
        assert_eq!(q.bindings.len(), 1);
        assert_eq!(linear_body(&q).statements.len(), 2);
    }

    /// What that costs. The clause governs the whole chain the
    /// statements go into, so a statement written behind the call, and
    /// therefore outside it, may only project what the call answered.
    #[test]
    fn a_read_behind_a_hoisted_use_is_refused() {
        let err = parse(
            "CALL { PROPERTY GRAPH g = CURRENT_PROPERTY_GRAPH USE g RETURN 1 AS n } \
             MATCH (p:Person) RETURN COUNT(*) AS c",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("may only project"), "{err}");
        // A statement that only projects cannot tell the two graphs
        // apart, so it is let through.
        assert!(
            parse("CALL { USE g RETURN 1 AS n } RETURN n AS n")
                .unwrap()
                .use_graph
                .is_some()
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
            alts,
            filter,
            ..
        } = &q.clauses()[0]
        else {
            panic!("first clause is MATCH");
        };
        assert!(!optional && filter.is_none() && alts.is_empty());
        let node = &patterns[0].start;
        assert_eq!(node.var.as_deref(), Some("n"));
        assert_eq!(node.label, Some(LabelExpr::Label("Person".into())));
        assert_eq!(node.props[0].0, "id");
        assert_eq!(node.props[0].1, Expr::Param("personId".into()));
        let projection = q.result().expect("RETURN");
        assert_eq!(projection.items[0].alias.as_deref(), Some("firstName"));
    }

    /// The bar sits inside one path pattern and the comma between two
    /// of them (ISO 16.7), so `A | B, C` is the two pattern lists
    /// `A, C` and `B, C` and not the one list `A` beside the list
    /// `B, C`. Reading it the other way round answers a different
    /// number of rows, so the shape is asserted rather than the count
    /// alone.
    #[test]
    fn a_bar_binds_tighter_than_a_comma() {
        let q = parsed("MATCH (a)-[:K]->(b) | (a)-[:W]->(b), (c) RETURN *");
        let Clause::Match {
            patterns,
            alts,
            distinct,
            ..
        } = &q.clauses()[0]
        else {
            panic!("MATCH");
        };
        assert!(distinct, "a single bar is the union and not the multiset");
        assert_eq!(alts.len(), 1, "two ways of matching");
        for way in std::iter::once(patterns).chain(alts) {
            assert_eq!(way.len(), 2, "each way is the alternative and the (c)");
            assert_eq!(way[1].start.var.as_deref(), Some("c"));
        }
        assert_eq!(patterns[0].steps[0].0.types, vec!["K".to_string()]);
        assert_eq!(alts[0][0].steps[0].0.types, vec!["W".to_string()]);
    }

    #[test]
    fn a_multiset_alternation_is_not_a_union() {
        let q = parsed("MATCH (a)-[:K]->(b) |+| (a)-[:W]->(b) RETURN *");
        let Clause::Match { alts, distinct, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        assert!(!distinct, "|+| keeps a path each way found");
        assert_eq!(alts.len(), 1);
    }

    /// The two bars mean different things about the same alternatives,
    /// so a pattern that wrote both has not said which of them it
    /// meant.
    #[test]
    fn the_two_bars_are_not_mixed_in_one_pattern() {
        let err = parse_err("MATCH (a)-[:K]->(b) | (a)-[:W]->(b) |+| (a)-[:X]->(b) RETURN *");
        assert!(err.contains("two answers at once"), "{err}");
    }

    /// Every comma multiplies the ways, so a long list of alternations
    /// is a walk count nobody wrote down. It is refused with the
    /// number and pointed at the composite query, which is the way to
    /// write a union that stays one walk per operand.
    #[test]
    fn too_many_ways_of_matching_are_refused() {
        let one = "(a)|(b)";
        let list = std::iter::repeat_n(one, 7).collect::<Vec<_>>().join(", ");
        let err = parse_err(&format!("MATCH {list} RETURN *"));
        assert!(err.contains("UNION"), "{err}");
    }

    /// A questioned stretch is the stretch walked and the stretch
    /// skipped, which are two shapes with different numbers of steps in
    /// them, so it reads as the two walks an alternation would have
    /// been written as. Skipping it leaves the nodes at its ends
    /// standing on the one node.
    #[test]
    fn a_questioned_stretch_reads_as_two_walks() {
        let q = parsed("MATCH (a) ((x)-[:K]->(y))? (b) RETURN *");
        let Clause::Match {
            patterns,
            alts,
            distinct,
            ..
        } = &q.clauses()[0]
        else {
            panic!("MATCH");
        };
        assert!(
            !distinct,
            "no bar was written, and two lengths cannot be the same path"
        );
        assert_eq!(alts.len(), 1, "the length nought and the length one");
        assert_eq!(patterns[0].steps.len(), 0, "the stretch skipped");
        assert_eq!(alts[0][0].steps.len(), 1, "the stretch walked");
        assert_eq!(patterns[0].start.var.as_deref(), Some("a"));
        assert_eq!(
            patterns[0].start.aliases,
            vec!["b".to_string()],
            "the ends of a skipped stretch meet at one node"
        );
    }

    /// A bounded range is a walk per length it holds, in order, and the
    /// lengths written out are the numbers of steps.
    #[test]
    fn a_bounded_range_reads_as_one_walk_per_length() {
        let q = parsed("MATCH (a) ((x)-[:K]->(y)){1,3} (b) RETURN *");
        let Clause::Match { patterns, alts, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        let lengths: Vec<usize> = std::iter::once(patterns)
            .chain(alts)
            .map(|way| way[0].steps.len())
            .collect();
        assert_eq!(lengths, vec![1, 2, 3]);
    }

    /// Two stretches of several lengths in one term are every pairing
    /// of their lengths, the last of them counting fastest.
    #[test]
    fn two_stretches_of_several_lengths_are_every_pairing() {
        let q = parsed("MATCH (a) ((x)-[:K]->(y))? (m) ((u)-[:W]->(v)){1,2} (b) RETURN *");
        let Clause::Match { patterns, alts, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        let lengths: Vec<usize> = std::iter::once(patterns)
            .chain(alts)
            .map(|way| way[0].steps.len())
            .collect();
        assert_eq!(lengths, vec![1, 2, 2, 3], "0+1, 0+2, 1+1 and 1+2");
    }

    /// The lengths are walked as patterns of their own, so a count with
    /// no ceiling on it has no list of patterns to walk.
    #[test]
    fn a_stretch_with_no_ceiling_on_its_count_is_refused() {
        for query in [
            "MATCH (a) ((x)-[:K]->(y))+ (b) RETURN *",
            "MATCH (a) ((x)-[:K]->(y))* (b) RETURN *",
            "MATCH (a) ((x)-[:K]->(y)){2,} (b) RETURN *",
        ] {
            let err = parse_err(query);
            assert!(err.contains("write a ceiling on the count"), "{err}");
        }
    }

    /// An INSERT describes one shape to make, and a stretch of several
    /// lengths describes several, so the reason it is refused for is
    /// the lengths rather than the bar it never wrote.
    #[test]
    fn a_stretch_of_several_lengths_is_refused_where_one_shape_belongs() {
        let err = parse_err("INSERT (a) ((x)-[:K]->(y))? (b)");
        assert!(err.contains("several lengths"), "{err}");
        assert!(!err.contains("bar"), "{err}");
    }

    #[test]
    fn directions_and_hop_ranges() {
        let q =
            parsed("MATCH (a)-[:KNOWS*1..2]->(b), (a)<-[r:LIKES|FOLLOWS]-(c), (b)-(c) RETURN *");
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
        ];
        for (pattern, want) in full.into_iter().chain(short) {
            let q = parsed(&format!("MATCH {pattern} RETURN a"));
            let Clause::Match { patterns, .. } = &q.clauses()[0] else {
                panic!("MATCH");
            };
            let (rel, _) = &patterns[0].steps[0];
            assert_eq!(rel.direction, want, "{pattern}");
        }
        // Cypher writes two of these with a second minus sign and GQL
        // does not, because in GQL a double minus opens a comment
        // (GB02). So the rest of the line goes with it and the
        // statement is short of a RETURN.
        for pattern in ["(a)--(b)", "(a)-->(b)"] {
            let e = parse_err(&format!("MATCH {pattern} RETURN a"));
            assert!(e.contains("42001"), "{pattern}: {e}");
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
            ("->+", (Some(1), None)),
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

    /// The set quantifier of a set function, both spellings and none.
    /// ALL is the default written down, so the three calls that do not
    /// say DISTINCT have to arrive as one and the same call.
    #[test]
    fn a_set_function_reads_its_set_quantifier() {
        for (source, want) in [
            ("count(n)", false),
            ("count(ALL n)", false),
            ("count(DISTINCT n)", true),
            ("collect_list(ALL n.age)", false),
            ("stddev_samp(ALL n.age)", false),
        ] {
            let q = parsed(&format!("MATCH (n) RETURN {source} AS v"));
            let Expr::Call { distinct, .. } = &q.result().expect("RETURN").items[0].expr else {
                panic!("{source} is a call");
            };
            assert_eq!(*distinct, want, "{source}");
        }
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

    /// The constructor may name the type it is building, ISO 20.17 and
    /// 20.19, and the name is worth nothing beyond saying what the
    /// reader is looking at: the three spellings are one tree.
    #[test]
    fn a_constructor_may_name_its_type() {
        let want = Expr::List(vec![
            Expr::Literal(Literal::Int(1)),
            Expr::Literal(Literal::Int(2)),
        ]);
        for source in ["RETURN [1, 2]", "RETURN LIST [1, 2]", "RETURN ARRAY [1, 2]"] {
            let q = parsed(source);
            assert_eq!(q.result().expect("RETURN").items[0].expr, want, "{source}");
        }
        let want = Expr::Map(vec![("a".to_string(), Expr::Literal(Literal::Int(1)))]);
        for source in ["RETURN {a: 1}", "RETURN RECORD {a: 1}"] {
            let q = parsed(source);
            assert_eq!(q.result().expect("RETURN").items[0].expr, want, "{source}");
        }
    }

    /// The type words are reserved (ISO 21.3), so a variable cannot be
    /// called one however the query is written: a variable is a plain
    /// name and a plain name is not a reserved word. What the bracket
    /// and the brace decide is which construct the word opens, and
    /// that question is asked of the word and not of a binding.
    #[test]
    fn a_type_word_is_reserved_and_is_not_a_variable() {
        for name in ["list", "array", "record"] {
            let e = parse_err(&format!("LET {name} = 1 RETURN {name}"));
            assert!(e.contains("reserved word"), "{name}: {e}");
        }
        // A word that is not reserved is still a name, which is what
        // says the refusal above is about the list and not about the
        // slot.
        let q = parsed("LET tally = 1 RETURN tally");
        assert_eq!(
            q.result().expect("RETURN").items[0].expr,
            Expr::Variable("tally".to_string())
        );
    }

    /// The deviation `expect_field_name` exists for: a reserved word
    /// is a property name and a field name and is nothing else.
    #[test]
    fn a_reserved_word_is_still_a_property_name() {
        for text in [
            "INSERT (:Thing {year: 2024})",
            "RETURN {year: 1} AS r",
            "MATCH (n:Thing) RETURN n.year AS y",
            "MATCH (n:Thing) SET n.year = 2024",
            "RETURN DATE({year: 2024, month: 1, day: 1}) AS v",
        ] {
            parsed(text);
        }
        for text in [
            "RETURN 1 AS year",
            "MATCH (year:Person) RETURN 1 AS n",
            "LET year = 1 RETURN year",
        ] {
            let e = parse_err(text);
            assert!(e.contains("reserved word"), "{text}: {e}");
        }
    }

    /// GV56 and GV57, both spellings of each, and GV50's maximum in
    /// the brackets ISO writes it in.
    #[test]
    fn a_reference_value_type_parses_open_and_closed() {
        let ty = |text: &str| {
            let q = parsed(&format!("RETURN CAST(x AS {text}) AS v"));
            match &q.result().expect("RETURN").items[0].expr {
                Expr::Cast { ty, .. } => ty.base().clone(),
                other => panic!("{text}: {other:?}"),
            }
        };
        assert_eq!(ty("NODE"), LogicalType::Node(None));
        assert_eq!(ty("ANY NODE"), LogicalType::Node(None));
        assert_eq!(ty("ANY VERTEX"), LogicalType::Node(None));
        assert_eq!(ty("EDGE"), LogicalType::Edge(None));
        assert_eq!(ty("ANY RELATIONSHIP"), LogicalType::Edge(None));
        assert_eq!(ty("NODE :Person"), LogicalType::Node(Some("Person".into())));
        assert_eq!(
            ty("NODE TYPE :Person"),
            LogicalType::Node(Some("Person".into()))
        );
        assert_eq!(
            ty("NODE (:Person)"),
            LogicalType::Node(Some("Person".into()))
        );
        assert_eq!(ty("EDGE :KNOWS"), LogicalType::Edge(Some("KNOWS".into())));
        assert_eq!(
            ty("LIST<INT>[2]"),
            ty("LIST<INT>(2)"),
            "the two spellings of a maximum are one type"
        );
    }

    #[test]
    fn call_parses_args_and_yield_aliases() {
        let q = parsed("CALL sssp('KNOWS', 42) YIELD node AS n, distance RETURN n, distance");
        let Clause::Call {
            at,
            proc,
            args,
            yields,
        } = &q.clauses()[0]
        else {
            panic!("CALL");
        };
        assert_eq!(*at, None);
        assert_eq!(proc.schema, None, "a bare name says no schema");
        assert_eq!(proc.name, "sssp");
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

    /// A procedure's name is a catalog object reference, so it may be
    /// written out as a path, and the schema it is looked up in may be
    /// said with AT instead. Both spellings reach the binder as a
    /// schema and a name, which is what makes them one thing there.
    #[test]
    fn a_procedure_is_named_by_a_path_or_by_the_schema_it_is_at() {
        let named = |source: &str| {
            let q = parsed(source);
            let Clause::Call { at, proc, .. } = &q.clauses()[0] else {
                panic!("CALL in {source}");
            };
            (at.clone(), proc.schema.clone(), proc.name.clone())
        };
        assert_eq!(
            named("CALL /pagerank('KNOWS') YIELD node, rank RETURN rank"),
            (None, Some("/".to_string()), "pagerank".to_string()),
            "one segment is a name in the root schema"
        );
        assert_eq!(
            named("CALL /algo/pagerank('KNOWS') YIELD node, rank RETURN rank"),
            (None, Some("/algo".to_string()), "pagerank".to_string())
        );
        assert_eq!(
            named("CALL AT /algo pagerank('KNOWS') YIELD node, rank RETURN rank"),
            (Some("/algo".to_string()), None, "pagerank".to_string())
        );
        assert_eq!(
            named("CALL AT / pagerank('KNOWS') YIELD node, rank RETURN rank"),
            (Some("/".to_string()), None, "pagerank".to_string()),
            "the root schema is one slash and nothing else"
        );
    }

    /// The word CALL starts two different clauses and the one token
    /// after it is what tells them apart, so the three ways an inline
    /// call is written are read here beside the table function they
    /// share a keyword with.
    #[test]
    fn an_inline_call_is_a_block_and_what_it_may_read() {
        for (source, want) in [
            ("MATCH (a) CALL { MATCH (b) RETURN b } RETURN a", None),
            (
                "MATCH (a) CALL (a) { MATCH (a)-[:knows]->(b) RETURN b } RETURN a",
                Some(vec!["a".to_string()]),
            ),
            (
                "MATCH (a) CALL () { MATCH (b) RETURN b } RETURN a",
                Some(Vec::new()),
            ),
        ] {
            let q = parsed(source);
            let Clause::CallInline { scope, body, .. } = &q.clauses()[1] else {
                panic!("an inline CALL in {source}");
            };
            assert_eq!(*scope, want, "{source}");
            assert!(body.result().is_some(), "{source}");
        }
    }

    /// The block is part of the text, so what the query names includes
    /// what the block names. The table an INSERT writes is declared off
    /// this walk, and a block is not a place a write hides in.
    #[test]
    fn the_clauses_of_a_query_include_the_clauses_of_a_block() {
        let q = parsed("MATCH (a:person) CALL (a) { INSERT (b:pet) } RETURN a");
        assert_eq!(q.clauses().len(), 3);
        assert!(
            q.clauses()
                .iter()
                .any(|c| matches!(c, Clause::Insert { .. })),
            "the INSERT inside the block"
        );
    }

    /// A block need not end with a RETURN, which is what makes it
    /// different from the query inside a VALUE or an EXISTS.
    #[test]
    fn a_block_that_returns_nothing_parses() {
        let q = parsed("MATCH (a:person) CALL (a) { INSERT (b:pet) } RETURN a");
        let Clause::CallInline { body, .. } = &q.clauses()[1] else {
            panic!("an inline CALL");
        };
        assert!(body.result().is_none());
    }

    /// GP03. The word in front of a call says the row is kept where the
    /// block answers nothing, and the same word in front of a match is
    /// a different clause, so what tells the two apart is the token
    /// behind the word and nothing else.
    #[test]
    fn an_optional_call_is_a_call_and_not_a_match() {
        for source in [
            "MATCH (a) OPTIONAL CALL (a) { MATCH (a)-[:knows]->(b) RETURN b AS f } RETURN a",
            "MATCH (a) OPTIONAL CALL { MATCH (b) RETURN b AS f } RETURN a",
        ] {
            let q = parsed(source);
            let Clause::CallInline { optional, .. } = &q.clauses()[1] else {
                panic!("an inline CALL in {source}");
            };
            assert!(*optional, "{source}");
        }
        let q = parsed("MATCH (a) OPTIONAL MATCH (a)-[:knows]->(b) RETURN a");
        assert!(
            matches!(&q.clauses()[1], Clause::Match { optional: true, .. }),
            "an OPTIONAL MATCH is still a match"
        );
        let q = parsed("MATCH (a) CALL (a) { MATCH (a)-[:knows]->(b) RETURN b AS f } RETURN a");
        let Clause::CallInline { optional, .. } = &q.clauses()[1] else {
            panic!("an inline CALL");
        };
        assert!(!optional, "a call written without the word");
    }

    /// A procedure named in the catalog answers a table of its own, so
    /// there is no row of the statement for the word to keep and it is
    /// refused where it is written rather than read and dropped.
    #[test]
    fn an_optional_call_of_a_named_procedure_is_refused() {
        let err = parse("OPTIONAL CALL pagerank('knows') YIELD node").unwrap_err();
        assert!(err.to_string().contains("OPTIONAL"), "{err}");
    }

    /// The word alone does not say which clause is coming, so a word
    /// with neither behind it is the match it always was, and the
    /// message says so rather than naming both.
    #[test]
    fn an_optional_that_begins_nothing_is_still_a_match() {
        let err = parse("MATCH (a) OPTIONAL RETURN a").unwrap_err();
        assert!(err.to_string().contains("MATCH"), "{err}");
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
        // Spaced, because two minus signs against each other are a
        // comment (GB02) and the whole line would go with them.
        let minuses = "- ".repeat(5000);
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

    /// ISO 21.3. An `<identifier>` is a regular identifier or a
    /// delimited one, and a delimited one is written in accent quotes
    /// or in double quotes, the two being the same production. A
    /// `<binding variable>` is a regular identifier only, so the
    /// delimited forms do not reach a variable however they are
    /// written, and a reserved word does not reach one either.
    #[test]
    fn a_name_may_be_delimited_and_a_variable_may_not() {
        for source in [
            "MATCH (u:`Unit`) RETURN 1 AS v",
            r#"MATCH (u:"Unit") RETURN 1 AS v"#,
            "RETURN 1 AS `MATCH`",
            r#"RETURN 1 AS "MATCH""#,
            "MATCH (u:Widget) RETURN u.`odd name` AS v",
        ] {
            parsed(source);
        }
        // A single quoted sequence is a string literal and only that,
        // so it does not stand where a name belongs.
        assert!(parse_err("RETURN 1 AS 'v'").contains("expected an alias"));
        for source in [
            "MATCH (`odd name`:Widget) RETURN 1 AS v",
            r#"MATCH ("odd name":Widget) RETURN 1 AS v"#,
        ] {
            let e = parse_err(source);
            assert!(e.contains("a variable is a plain name"), "{source}: {e}");
        }
        assert!(parse_err("RETURN 1 AS MATCH").contains("reserved word"));
        // A pre-reserved word is a reserved word, 21.3 writing the one
        // list as an alternative of the other, so it is no name either.
        assert!(parse_err("RETURN 1 AS data").contains("reserved word"));
        assert!(parse_err("MATCH (u:Unit) RETURN 1 AS v").contains("reserved word"));
    }

    /// GQ18. A value query expression carries a whole query: it may
    /// chain, sort and cut, and it is the brace that makes it one.
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
        // The brace is what says a value query, and the word without
        // one is refused for being reserved rather than read as a
        // variable, which is ISO 21.3 and not this expression's rule.
        assert!(
            parse_err("MATCH (value:Person) RETURN value.id AS id").contains("reserved word"),
            "VALUE is reserved"
        );
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
    fn only_a_bracket_makes_exists_the_predicate() {
        // An ordinary name in the same slot is an ordinary variable,
        // which is what says the predicate is the bracket's doing.
        // EXISTS itself is a reserved word and is no name at all.
        let q = parsed("MATCH (present:Person) RETURN present.id AS id");
        let Clause::Match { patterns, .. } = &q.clauses()[0] else {
            panic!("MATCH");
        };
        assert_eq!(patterns[0].start.var.as_deref(), Some("present"));
        assert!(
            parse_err("MATCH (exists:Person) RETURN exists.id AS id").contains("reserved word"),
            "EXISTS is reserved"
        );
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

    /// What makes a DELETE item a query is the brace and not the word
    /// in front of it, so an ordinary name in that position is an
    /// ordinary target. VALUE itself is reserved and cannot be the
    /// name, which is the rule and not this clause's doing.
    #[test]
    fn a_name_in_front_of_no_brace_is_read_as_a_variable() {
        let q = parsed("MATCH (holder:person) DELETE holder");
        let Clause::Delete { targets, .. } = &q.clauses()[1] else {
            panic!("DELETE");
        };
        assert_eq!(vars(targets), ["holder"]);
        assert!(
            parse_err("MATCH (value:person) DELETE value").contains("reserved word"),
            "VALUE is reserved"
        );
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
