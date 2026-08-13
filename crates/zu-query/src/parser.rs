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
use zu_common::{LogicalType, Result, ZuError};

use crate::ast::{
    BinaryOp, Clause, Expr, Literal, NodePattern, PathMode, PathPattern, Projection,
    ProjectionItem, Query, RelDirection, RelPattern, Selector, UnaryOp,
};
use crate::lexer::{Token, TokenKind, lex, position};
use crate::value_type;

/// A name written where a value type belongs and spelling none.
fn unknown_type(name: &str) -> ZuError {
    ZuError::gql(codes::C42001, format!("unknown value type '{name}'"))
}

/// Hard cap on expression nesting; hostile input past it errors instead
/// of overflowing the parser's stack.
const MAX_DEPTH: usize = 128;

/// Clause keywords the surface reserves but the v0 core does not parse
/// yet; naming them beats "expected MATCH" when someone writes CREATE.
const UNIMPLEMENTED: &[&str] = &[
    "CREATE", "SET", "DELETE", "DETACH", "MERGE", "FILTER", "LET", "NEXT",
];

/// Parses one zuQL query.
pub fn parse(source: &str) -> Result<Query> {
    let tokens = lex(source)?;
    let mut parser = Parser {
        source,
        tokens,
        pos: 0,
        depth: 0,
    };
    parser.parse_query()
}

struct Parser<'a> {
    source: &'a str,
    tokens: Vec<Token>,
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn error(&self, expected: &str) -> ZuError {
        let detail = match self.peek() {
            Some(token) => format!(
                "{}: expected {expected}, found {}",
                position(self.source, token.start),
                token.kind.describe()
            ),
            None => format!("unexpected end of query, expected {expected}"),
        };
        ZuError::gql(codes::C42001, detail)
    }

    /// True when the next token is the given keyword, case-insensitive.
    fn at_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Token { kind: TokenKind::Ident(s), .. }) if s.eq_ignore_ascii_case(kw))
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

    fn parse_query(&mut self) -> Result<Query> {
        let mut clauses = Vec::new();
        loop {
            if self.at_kw("MATCH") || self.at_kw("OPTIONAL") {
                let optional = self.eat_kw("OPTIONAL");
                self.expect_kw("MATCH")?;
                let mut patterns = vec![self.parse_path()?];
                while self.eat(&TokenKind::Comma) {
                    patterns.push(self.parse_path()?);
                }
                let filter = self.parse_where()?;
                clauses.push(Clause::Match {
                    optional,
                    patterns,
                    filter,
                });
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
                let expr = self.parse_expr()?;
                self.expect_kw("AS")?;
                let alias = self.expect_name("an alias after AS")?;
                clauses.push(Clause::Unwind { expr, alias });
            } else if self.eat_kw("WITH") {
                let projection = self.parse_projection()?;
                let filter = self.parse_where()?;
                clauses.push(Clause::With { projection, filter });
            } else if self.eat_kw("RETURN") {
                let projection = self.parse_projection()?;
                clauses.push(Clause::Return { projection });
                self.eat(&TokenKind::Semicolon);
                if let Some(token) = self.peek() {
                    return Err(ZuError::gql(
                        codes::C42001,
                        format!(
                            "{}: nothing may follow RETURN, found {}",
                            position(self.source, token.start),
                            token.kind.describe()
                        ),
                    ));
                }
                return Ok(Query { clauses });
            } else if let Some(kw) = UNIMPLEMENTED.iter().find(|kw| self.at_kw(kw)) {
                return Err(ZuError::gql(
                    codes::C42001,
                    format!(
                        "{}: {kw} is not implemented yet, the v0 core is MATCH, WHERE, CALL, UNWIND, WITH, RETURN",
                        position(self.source, self.peek().expect("peeked").start)
                    ),
                ));
            } else if clauses.is_empty() && self.peek().is_none() {
                return Err(ZuError::gql(codes::C42001, "empty query"));
            } else {
                return Err(self.error("MATCH, OPTIONAL MATCH, CALL, UNWIND, WITH, or RETURN"));
            }
        }
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
                order_by.push((expr, ascending));
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let skip = if self.eat_kw("SKIP") {
            Some(self.parse_expr()?)
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
        let selector = if self.eat_kw("ANY") {
            self.expect_kw("SHORTEST")?;
            Some(Selector::AnyShortest)
        } else if self.eat_kw("ALL") {
            self.expect_kw("SHORTEST")?;
            Some(Selector::AllShortest)
        } else if self.at_kw("SHORTEST") {
            return Err(ZuError::gql(
                codes::C42001,
                "SHORTEST needs a quantity: write ANY SHORTEST or ALL SHORTEST",
            ));
        } else {
            None
        };
        let mode = if self.eat_kw("WALK") {
            PathMode::Walk
        } else if self.eat_kw("TRAIL") {
            PathMode::Trail
        } else if self.eat_kw("ACYCLIC") {
            PathMode::Acyclic
        } else if self.at_kw("SIMPLE") {
            return Err(ZuError::gql(
                codes::C42001,
                "the SIMPLE path mode is not supported yet; use ACYCLIC",
            ));
        } else {
            PathMode::default()
        };
        let start = self.parse_node()?;
        let mut steps = Vec::new();
        while self.at(&TokenKind::Minus) || self.at(&TokenKind::Lt) {
            let rel = self.parse_rel()?;
            let node = self.parse_node()?;
            steps.push((rel, node));
        }
        Ok(PathPattern {
            var,
            selector,
            mode,
            start,
            steps,
        })
    }

    fn parse_node(&mut self) -> Result<NodePattern> {
        self.expect(&TokenKind::LParen)?;
        let var = match self.peek().map(|t| &t.kind) {
            Some(TokenKind::Ident(_)) | Some(TokenKind::QuotedIdent(_)) => {
                Some(self.expect_name("a variable")?)
            }
            _ => None,
        };
        let mut labels = Vec::new();
        while self.eat(&TokenKind::Colon) {
            labels.push(self.expect_name("a label name")?);
        }
        let props = if self.at(&TokenKind::LBrace) {
            self.parse_property_map()?
        } else {
            Vec::new()
        };
        self.expect(&TokenKind::RParen)?;
        Ok(NodePattern { var, labels, props })
    }

    /// Parses the relationship between two nodes. `<-` and `->` are not
    /// lexer tokens, so the arrows are assembled here from `<`, `-`,
    /// and `>`.
    fn parse_rel(&mut self) -> Result<RelPattern> {
        let inbound = self.eat(&TokenKind::Lt);
        self.expect(&TokenKind::Minus)?;
        let (var, types, range, props) = if self.eat(&TokenKind::LBracket) {
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
            self.expect(&TokenKind::RBracket)?;
            (var, types, range, props)
        } else {
            (None, Vec::new(), None, Vec::new())
        };
        self.expect(&TokenKind::Minus)?;
        let outbound = self.eat(&TokenKind::Gt);
        let direction = match (inbound, outbound) {
            (true, true) => {
                return Err(ZuError::gql(
                    codes::C42001,
                    format!(
                        "{}: a relationship cannot point both ways",
                        position(self.source, self.tokens[self.pos - 1].start)
                    ),
                ));
            }
            (true, false) => RelDirection::In,
            (false, true) => RelDirection::Out,
            (false, false) => RelDirection::Undirected,
        };
        Ok(RelPattern {
            var,
            types,
            direction,
            range,
            props,
        })
    }

    /// The hop range after `*`: nothing, `2`, `1..3`, `..3`, or `2..`.
    fn parse_hop_range(&mut self) -> Result<(Option<u64>, Option<u64>)> {
        let take_int = |parser: &mut Self| -> Option<u64> {
            if let Some(Token {
                kind: TokenKind::Int(v),
                ..
            }) = parser.peek()
            {
                let v = *v;
                parser.pos += 1;
                Some(v)
            } else {
                None
            }
        };
        let min = take_int(self);
        if self.eat(&TokenKind::DotDot) {
            Ok((min, take_int(self)))
        } else {
            // `*2` is exactly two hops; a bare `*` is unbounded.
            Ok((min, min))
        }
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
        let mut lhs = self.parse_additive()?;
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
                        let rhs = self.parse_additive()?;
                        lhs = binary(BinaryOp::In, lhs, rhs);
                        continue;
                    }
                    if self.at_kw("STARTS") {
                        self.pos += 1;
                        self.expect_kw("WITH")?;
                        let rhs = self.parse_additive()?;
                        lhs = binary(BinaryOp::StartsWith, lhs, rhs);
                        continue;
                    }
                    if self.at_kw("ENDS") {
                        self.pos += 1;
                        self.expect_kw("WITH")?;
                        let rhs = self.parse_additive()?;
                        lhs = binary(BinaryOp::EndsWith, lhs, rhs);
                        continue;
                    }
                    if self.eat_kw("CONTAINS") {
                        let rhs = self.parse_additive()?;
                        lhs = binary(BinaryOp::Contains, lhs, rhs);
                        continue;
                    }
                    if self.at_kw("IS") {
                        self.pos += 1;
                        let negated = self.eat_kw("NOT");
                        self.expect_kw("NULL")?;
                        lhs = Expr::IsNull {
                            expr: Box::new(lhs),
                            negated,
                        };
                        continue;
                    }
                    break;
                }
            };
            self.pos += 1;
            let rhs = self.parse_additive()?;
            lhs = binary(op, lhs, rhs);
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
                    ZuError::gql(
                        codes::C22003,
                        format!(
                            "{}: integer literal out of range",
                            position(self.source, token.start)
                        ),
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
                if name.eq_ignore_ascii_case("null") {
                    self.pos += 1;
                    return Ok(Expr::Literal(Literal::Null));
                }
                if name.eq_ignore_ascii_case("true") {
                    self.pos += 1;
                    return Ok(Expr::Literal(Literal::Bool(true)));
                }
                if name.eq_ignore_ascii_case("false") {
                    self.pos += 1;
                    return Ok(Expr::Literal(Literal::Bool(false)));
                }
                let name = name.clone();
                self.pos += 1;
                if self.at(&TokenKind::LParen) {
                    // CAST is written like a call and is not one: its
                    // second argument is a type, which no expression
                    // grammar can produce, so it is taken here rather
                    // than left to a function that would receive a
                    // variable named INT8.
                    if name.eq_ignore_ascii_case("CAST") {
                        self.parse_cast()
                    } else {
                        self.parse_call(name)
                    }
                } else {
                    Ok(Expr::Variable(name))
                }
            }
            TokenKind::QuotedIdent(ref name) => {
                let name = name.clone();
                self.pos += 1;
                Ok(Expr::Variable(name))
            }
            _ => Err(self.error("an expression")),
        }
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

    /// A value type name, with its optional precision and its optional
    /// `NOT NULL`.
    ///
    /// A target without `NOT NULL` is nullable, which is the standard's
    /// default and not a convenience: `CAST(NULL AS INT)` has to be
    /// null rather than an error, or every optional property that ever
    /// meets a cast becomes one.
    fn parse_value_type(&mut self) -> Result<LogicalType> {
        let name = match self.peek() {
            Some(Token {
                kind: TokenKind::Ident(s),
                ..
            }) => s.clone(),
            _ => return Err(self.error("a value type")),
        };
        self.pos += 1;
        let ty = if self.eat(&TokenKind::LParen) {
            let precision = match self.peek() {
                Some(Token {
                    kind: TokenKind::Int(v),
                    ..
                }) => u16::try_from(*v).map_err(|_| self.error("a precision in digits"))?,
                _ => return Err(self.error("a precision in digits")),
            };
            self.pos += 1;
            self.expect(&TokenKind::RParen)?;
            // Two messages, because the two mistakes are different: a
            // name nobody knows and a name that knows no precision.
            if value_type::by_name(&name).is_none() {
                return Err(unknown_type(&name));
            }
            value_type::by_name_with_precision(&name, precision).ok_or_else(|| {
                ZuError::gql(
                    codes::C42001,
                    format!(
                        "'{name}' does not take a precision, or {precision} is too many digits"
                    ),
                )
            })?
        } else {
            value_type::by_name(&name).ok_or_else(|| unknown_type(&name))?
        };
        if self.eat_kw("NOT") {
            self.expect_kw("NULL")?;
            return Ok(ty);
        }
        Ok(LogicalType::Nullable(Box::new(ty)))
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

    fn parsed(source: &str) -> Query {
        parse(source).expect("parse")
    }

    fn parse_err(source: &str) -> String {
        parse(source).expect_err("should fail").to_string()
    }

    #[test]
    fn point_lookup_shape() {
        // LDBC short-read shape: one labeled node with a param prop.
        let q = parsed("MATCH (n:Person {id: $personId}) RETURN n.firstName AS firstName");
        assert_eq!(q.clauses.len(), 2);
        let Clause::Match {
            optional,
            patterns,
            filter,
        } = &q.clauses[0]
        else {
            panic!("first clause is MATCH");
        };
        assert!(!optional && filter.is_none());
        let node = &patterns[0].start;
        assert_eq!(node.var.as_deref(), Some("n"));
        assert_eq!(node.labels, ["Person"]);
        assert_eq!(node.props[0].0, "id");
        assert_eq!(node.props[0].1, Expr::Param("personId".into()));
        let Clause::Return { projection } = &q.clauses[1] else {
            panic!("second clause is RETURN");
        };
        assert_eq!(projection.items[0].alias.as_deref(), Some("firstName"));
    }

    #[test]
    fn directions_and_hop_ranges() {
        let q =
            parsed("MATCH (a)-[:KNOWS*1..2]->(b), (a)<-[r:LIKES|FOLLOWS]-(c), (b)--(c) RETURN *");
        let Clause::Match { patterns, .. } = &q.clauses[0] else {
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
        assert_eq!(bare.direction, RelDirection::Undirected);
        assert!(bare.range.is_none());
        let Clause::Return { projection } = &q.clauses[1] else {
            panic!("RETURN");
        };
        assert!(projection.star);
    }

    #[test]
    fn path_modes_and_selectors_parse() {
        let q = parsed(
            "MATCH p = ANY SHORTEST TRAIL (a)-[:KNOWS*]->(b), \
             ALL SHORTEST (a)-[:KNOWS*]->(c), \
             WALK (a)-[:KNOWS*1..2]->(d), \
             ACYCLIC (a)-[:KNOWS*]->(e) RETURN *",
        );
        let Clause::Match { patterns, .. } = &q.clauses[0] else {
            panic!("MATCH");
        };
        assert_eq!(patterns[0].var.as_deref(), Some("p"));
        assert_eq!(patterns[0].selector, Some(Selector::AnyShortest));
        assert_eq!(patterns[0].mode, PathMode::Trail);
        assert_eq!(patterns[1].selector, Some(Selector::AllShortest));
        assert_eq!(patterns[1].mode, PathMode::Trail);
        assert_eq!(patterns[2].selector, None);
        assert_eq!(patterns[2].mode, PathMode::Walk);
        assert_eq!(patterns[3].mode, PathMode::Acyclic);
    }

    #[test]
    fn bare_shortest_and_simple_read_as_errors() {
        assert!(
            parse_err("MATCH SHORTEST (a)-[:KNOWS*]->(b) RETURN *")
                .contains("ANY SHORTEST or ALL SHORTEST")
        );
        assert!(parse_err("MATCH SIMPLE (a)-[:KNOWS*]->(b) RETURN *").contains("use ACYCLIC"));
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
            let Clause::Match { patterns, .. } = &q.clauses[0] else {
                panic!("MATCH");
            };
            assert_eq!(patterns[0].steps[0].0.range, Some(want), "range {text}");
        }
    }

    #[test]
    fn with_aggregation_pipeline() {
        let q = parsed(
            "MATCH (a:Person)-[:KNOWS]->(b) WITH a, count(b) AS friends WHERE friends > 5 \
             RETURN DISTINCT a.name, friends ORDER BY friends DESC, a.name SKIP 2 LIMIT 10",
        );
        let Clause::With { projection, filter } = &q.clauses[1] else {
            panic!("WITH");
        };
        assert!(filter.is_some());
        let Expr::Call { name, star, .. } = &projection.items[1].expr else {
            panic!("count call");
        };
        assert_eq!(name, "count");
        assert!(!star);
        let Clause::Return { projection } = &q.clauses[2] else {
            panic!("RETURN");
        };
        assert!(projection.distinct);
        assert_eq!(projection.order_by.len(), 2);
        assert!(!projection.order_by[0].1, "DESC");
        assert!(projection.order_by[1].1, "implicit ASC");
        assert_eq!(projection.skip, Some(Expr::Literal(Literal::Int(2))));
        assert_eq!(projection.limit, Some(Expr::Literal(Literal::Int(10))));
    }

    #[test]
    fn unwind_and_lists() {
        let q = parsed("UNWIND [1, 2, 3] AS x RETURN x * -1");
        let Clause::Unwind { expr, alias } = &q.clauses[0] else {
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
        let Clause::Return { projection } = &q.clauses[1] else {
            panic!("RETURN");
        };
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
        let Clause::Call { name, args, yields } = &q.clauses[0] else {
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
        } = &q.clauses[0]
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
    fn optional_match_and_path_binding() {
        let q =
            parsed("MATCH p = (a)-[:KNOWS]->(b) OPTIONAL MATCH (b)-[:WORKS_AT]->(c) RETURN p, c");
        let Clause::Match {
            optional, patterns, ..
        } = &q.clauses[0]
        else {
            panic!("MATCH");
        };
        assert!(!optional);
        assert_eq!(patterns[0].var.as_deref(), Some("p"));
        let Clause::Match { optional, .. } = &q.clauses[1] else {
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
        assert!(parse_err("MATCH (a)<-[r]->(b) RETURN a").contains("cannot point both ways"));
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
    fn keywords_are_case_insensitive() {
        let q = parsed("match (n:Person) where n.id = 1 return n limit 1");
        assert_eq!(q.clauses.len(), 2);
        let Clause::Return { projection } = &q.clauses[1] else {
            panic!("RETURN");
        };
        assert!(projection.limit.is_some());
    }
}
