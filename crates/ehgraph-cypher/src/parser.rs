//! Recursive-descent parser for the openCypher subset.
//!
//! Grammar follows the openCypher 9 EBNF (opencypher.org/resources) for the
//! clauses listed in `docs/wiki/Cypher.md`; anything else is a syntax error that
//! names the unsupported construct rather than a silent misparse.

use crate::ast::*;
use crate::lexer::{lex, Tok, Token};
use crate::value::Val;
use crate::CypherError;

pub fn parse(src: &str) -> Result<Query, CypherError> {
    let tokens = lex(src)?;
    let mut p = Parser {
        toks: tokens,
        i: 0,
        src: src.chars().collect(),
    };
    let mut query = p.query()?;
    while p.eat_kw("UNION") {
        let all = p.eat_kw("ALL");
        let part = p.query()?;
        query.unions.push((all, part.clauses));
    }
    if let Some((first, _)) = query.unions.first() {
        if query.unions.iter().any(|(all, _)| all != first) {
            return Err(CypherError::syntax(
                "UNION and UNION ALL cannot be mixed in one query",
            ));
        }
    }
    while p.eat(&Tok::Semi) {}
    if !p.at(&Tok::Eof) {
        return Err(p.unexpected("end of query"));
    }
    crate::check::check(&mut query)?;
    Ok(query)
}

struct Parser {
    toks: Vec<Token>,
    i: usize,
    src: Vec<char>,
}

/// Keywords that end an expression list or start a clause.
const CLAUSE_KEYWORDS: [&str; 15] = [
    "MATCH", "OPTIONAL", "WHERE", "RETURN", "WITH", "UNWIND", "CREATE", "MERGE", "SET", "REMOVE",
    "DELETE", "DETACH", "ORDER", "SKIP", "LIMIT",
];

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.i].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.i + n).min(self.toks.len() - 1)].tok
    }

    fn at(&self, tok: &Tok) -> bool {
        self.peek() == tok
    }

    fn bump(&mut self) -> Tok {
        let tok = self.toks[self.i].tok.clone();
        if self.i < self.toks.len() - 1 {
            self.i += 1;
        }
        tok
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.at(tok) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: &Tok, what: &str) -> Result<(), CypherError> {
        if self.eat(tok) {
            Ok(())
        } else {
            Err(self.unexpected(what))
        }
    }

    fn unexpected(&self, what: &str) -> CypherError {
        let t = &self.toks[self.i];
        let found = match &t.tok {
            Tok::Eof => "end of input".to_string(),
            Tok::Ident { name, .. } => format!("'{name}'"),
            other => format!("{other:?}"),
        };
        CypherError::syntax(format!(
            "expected {what}, found {found} at line {}, column {}",
            t.line, t.col
        ))
    }

    fn is_kw_at(&self, n: usize, kw: &str) -> bool {
        matches!(self.peek_at(n), Tok::Ident { name, quoted: false } if name.eq_ignore_ascii_case(kw))
    }

    fn is_kw(&self, kw: &str) -> bool {
        self.is_kw_at(0, kw)
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<(), CypherError> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.unexpected(kw))
        }
    }

    fn ident(&mut self, what: &str) -> Result<String, CypherError> {
        match self.peek().clone() {
            Tok::Ident { name, .. } => {
                self.bump();
                Ok(name)
            }
            _ => Err(self.unexpected(what)),
        }
    }

    /// A symbolic name that is not a clause keyword (variables).
    fn variable(&mut self) -> Result<String, CypherError> {
        if let Tok::Ident {
            name,
            quoted: false,
        } = self.peek()
        {
            if CLAUSE_KEYWORDS.iter().any(|k| name.eq_ignore_ascii_case(k)) {
                return Err(self.unexpected("a variable"));
            }
        }
        self.ident("a variable")
    }

    fn query(&mut self) -> Result<Query, CypherError> {
        let mut clauses = Vec::new();
        loop {
            if self.at(&Tok::Eof) || self.at(&Tok::Semi) {
                break;
            }
            if self.is_kw("UNION") {
                break;
            }
            if self.is_kw("CALL") {
                return Err(CypherError::unsupported("CALL (procedures and subqueries)"));
            }
            if self.is_kw("FOREACH") {
                return Err(CypherError::unsupported("FOREACH"));
            }
            if self.is_kw("LOAD") {
                return Err(CypherError::unsupported("LOAD CSV"));
            }
            let clause = self.clause()?;
            let is_return = matches!(clause, Clause::Return(_));
            clauses.push(clause);
            if is_return {
                break;
            }
        }
        if clauses.is_empty() {
            return Err(CypherError::syntax("empty query".to_string()));
        }
        let last = clauses.last().expect("non-empty");
        let ends_ok = matches!(
            last,
            Clause::Return(_)
                | Clause::Create(_)
                | Clause::Merge { .. }
                | Clause::Set(_)
                | Clause::Remove(_)
                | Clause::Delete { .. }
        );
        if !ends_ok {
            return Err(CypherError::syntax(
                "a query must end with RETURN or an updating clause".to_string(),
            ));
        }
        Ok(Query {
            clauses,
            unions: Vec::new(),
        })
    }

    fn clause(&mut self) -> Result<Clause, CypherError> {
        if self.is_kw("OPTIONAL") {
            self.bump();
            self.expect_kw("MATCH")?;
            return self.match_body(true);
        }
        if self.eat_kw("MATCH") {
            return self.match_body(false);
        }
        if self.eat_kw("UNWIND") {
            let expr = self.expr()?;
            self.expect_kw("AS")?;
            let alias = self.variable()?;
            return Ok(Clause::Unwind { expr, alias });
        }
        if self.eat_kw("WITH") {
            return Ok(Clause::With(self.projection(true)?));
        }
        if self.eat_kw("RETURN") {
            return Ok(Clause::Return(self.projection(false)?));
        }
        if self.eat_kw("CREATE") {
            return Ok(Clause::Create(self.pattern()?));
        }
        if self.eat_kw("MERGE") {
            let part = self.pattern_part()?;
            let mut on_create = Vec::new();
            let mut on_match = Vec::new();
            while self.is_kw("ON") {
                self.bump();
                if self.eat_kw("CREATE") {
                    self.expect_kw("SET")?;
                    on_create.extend(self.set_items()?);
                } else if self.eat_kw("MATCH") {
                    self.expect_kw("SET")?;
                    on_match.extend(self.set_items()?);
                } else {
                    return Err(self.unexpected("CREATE or MATCH after ON"));
                }
            }
            return Ok(Clause::Merge {
                part,
                on_create,
                on_match,
            });
        }
        if self.eat_kw("SET") {
            return Ok(Clause::Set(self.set_items()?));
        }
        if self.eat_kw("REMOVE") {
            let mut items = Vec::new();
            loop {
                let var = self.variable()?;
                if self.eat(&Tok::Dot) {
                    let key = self.ident("a property key")?;
                    items.push(RemoveItem::Prop { var, key });
                } else if self.at(&Tok::Colon) {
                    items.push(RemoveItem::Labels {
                        var,
                        labels: self.labels()?,
                    });
                } else {
                    return Err(self.unexpected(". or : after REMOVE target"));
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            return Ok(Clause::Remove(items));
        }
        let detach = self.eat_kw("DETACH");
        if self.eat_kw("DELETE") {
            let mut exprs = vec![self.expr()?];
            while self.eat(&Tok::Comma) {
                exprs.push(self.expr()?);
            }
            return Ok(Clause::Delete { detach, exprs });
        }
        if detach {
            return Err(self.unexpected("DELETE after DETACH"));
        }
        Err(self.unexpected("a clause (MATCH, OPTIONAL MATCH, UNWIND, WITH, RETURN, CREATE, MERGE, SET, REMOVE, DELETE)"))
    }

    fn match_body(&mut self, optional: bool) -> Result<Clause, CypherError> {
        let patterns = self.pattern()?;
        let where_ = if self.eat_kw("WHERE") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Clause::Match {
            optional,
            patterns,
            where_,
        })
    }

    fn set_items(&mut self) -> Result<Vec<SetItem>, CypherError> {
        let mut items = Vec::new();
        loop {
            // `SET (n).k = v` is the same as `SET n.k = v`.
            let parenthesized = self.at(&Tok::LParen)
                && matches!(self.peek_at(1), Tok::Ident { .. })
                && self.peek_at(2) == &Tok::RParen;
            if parenthesized {
                self.bump();
            }
            let var = self.variable()?;
            if parenthesized {
                self.bump();
            }
            if self.eat(&Tok::Dot) {
                let key = self.ident("a property key")?;
                self.expect(&Tok::Eq, "=")?;
                let value = self.expr()?;
                items.push(SetItem::Prop { var, key, value });
            } else if self.eat(&Tok::Eq) {
                items.push(SetItem::Replace {
                    var,
                    value: self.expr()?,
                });
            } else if self.eat(&Tok::PlusEq) {
                items.push(SetItem::Merge {
                    var,
                    value: self.expr()?,
                });
            } else if self.at(&Tok::Colon) {
                items.push(SetItem::Labels {
                    var,
                    labels: self.labels()?,
                });
            } else {
                return Err(self.unexpected("., =, += or : in SET"));
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(items)
    }

    fn projection(&mut self, is_with: bool) -> Result<Projection, CypherError> {
        let distinct = self.eat_kw("DISTINCT");
        let mut star = false;
        let mut items = Vec::new();
        if self.eat(&Tok::Star) {
            star = true;
            if !self.eat(&Tok::Comma) {
                return self.projection_tail(distinct, star, items, is_with);
            }
        }
        loop {
            let start = self.toks[self.i].pos;
            let expr = self.expr()?;
            let end = self.toks[self.i].pos;
            let alias = if self.eat_kw("AS") {
                self.variable()?
            } else {
                match &expr {
                    Expr::Var(name) => name.clone(),
                    _ => {
                        if is_with {
                            return Err(CypherError::syntax(
                                "expressions in WITH must be aliased (use AS)".to_string(),
                            ));
                        }
                        self.src[start..end]
                            .iter()
                            .collect::<String>()
                            .trim()
                            .to_string()
                    }
                }
            };
            items.push((expr, alias));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.projection_tail(distinct, star, items, is_with)
    }

    fn projection_tail(
        &mut self,
        distinct: bool,
        star: bool,
        items: Vec<(Expr, String)>,
        is_with: bool,
    ) -> Result<Projection, CypherError> {
        let mut order = Vec::new();
        if self.is_kw("ORDER") {
            self.bump();
            self.expect_kw("BY")?;
            loop {
                let e = self.expr()?;
                let asc = if self.eat_kw("DESC") || self.eat_kw("DESCENDING") {
                    false
                } else {
                    let _ = self.eat_kw("ASC") || self.eat_kw("ASCENDING");
                    true
                };
                order.push((e, asc));
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let skip = if self.eat_kw("SKIP") {
            Some(self.expr()?)
        } else {
            None
        };
        let limit = if self.eat_kw("LIMIT") {
            Some(self.expr()?)
        } else {
            None
        };
        let where_ = if is_with && self.eat_kw("WHERE") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Projection {
            distinct,
            star,
            items,
            order,
            skip,
            limit,
            where_,
        })
    }

    // ---- patterns ----

    fn pattern(&mut self) -> Result<Vec<PatternPart>, CypherError> {
        let mut parts = vec![self.pattern_part()?];
        while self.eat(&Tok::Comma) {
            parts.push(self.pattern_part()?);
        }
        Ok(parts)
    }

    fn pattern_part(&mut self) -> Result<PatternPart, CypherError> {
        let mut path_var = None;
        if matches!(self.peek(), Tok::Ident { .. }) && self.peek_at(1) == &Tok::Eq {
            path_var = Some(self.variable()?);
            self.bump();
        }
        let shortest = if self.is_kw("shortestPath") && self.peek_at(1) == &Tok::LParen {
            Some(false)
        } else if self.is_kw("allShortestPaths") && self.peek_at(1) == &Tok::LParen {
            Some(true)
        } else {
            None
        };
        if shortest.is_some() {
            self.bump();
            self.expect(&Tok::LParen, "(")?;
        }
        let start = self.node_pat()?;
        let mut chain = Vec::new();
        while self.at(&Tok::Minus) || self.at(&Tok::Lt) {
            let rel = self.rel_pat()?;
            let node = self.node_pat()?;
            chain.push((rel, node));
        }
        if shortest.is_some() {
            self.expect(&Tok::RParen, ")")?;
            if chain.len() != 1 {
                return Err(CypherError::syntax(
                    "shortestPath needs exactly one relationship pattern".to_string(),
                ));
            }
        }
        Ok(PatternPart {
            path_var,
            shortest,
            start,
            chain,
        })
    }

    fn labels(&mut self) -> Result<Vec<String>, CypherError> {
        let mut labels = Vec::new();
        while self.eat(&Tok::Colon) {
            labels.push(self.ident("a label")?);
        }
        Ok(labels)
    }

    fn node_pat(&mut self) -> Result<NodePat, CypherError> {
        self.expect(&Tok::LParen, "( to start a node pattern")?;
        let var = if matches!(self.peek(), Tok::Ident { .. }) {
            Some(self.variable()?)
        } else {
            None
        };
        let labels = self.labels()?;
        let props = self.pattern_props()?;
        self.expect(&Tok::RParen, ") to end a node pattern")?;
        Ok(NodePat { var, labels, props })
    }

    fn pattern_props(&mut self) -> Result<Option<Expr>, CypherError> {
        if self.at(&Tok::LBrace) {
            return Ok(Some(self.map_literal()?));
        }
        if let Tok::Param(name) = self.peek().clone() {
            self.bump();
            return Ok(Some(Expr::Param(name)));
        }
        Ok(None)
    }

    fn rel_pat(&mut self) -> Result<RelPat, CypherError> {
        let left = self.eat(&Tok::Lt);
        self.expect(&Tok::Minus, "-")?;
        let mut var = None;
        let mut types = Vec::new();
        let mut props = None;
        let mut range = None;
        if self.eat(&Tok::LBracket) {
            if matches!(self.peek(), Tok::Ident { .. }) {
                var = Some(self.variable()?);
            }
            if self.eat(&Tok::Colon) {
                types.push(self.ident("a relationship type")?);
                while self.eat(&Tok::Pipe) {
                    let _ = self.eat(&Tok::Colon);
                    types.push(self.ident("a relationship type")?);
                }
            }
            if self.eat(&Tok::Star) {
                let mut min = 1;
                let mut max = None;
                if let Tok::Int(n) = *self.peek() {
                    self.bump();
                    min = n.max(0) as u32;
                    max = Some(min);
                }
                if self.eat(&Tok::DotDot) {
                    max = None;
                    if let Tok::Int(n) = *self.peek() {
                        self.bump();
                        max = Some(n.max(0) as u32);
                    }
                }
                // `*` = 1.., `*n` = n..n, `*a..b`, `*..b` = 1..b, `*a..` = a..
                range = Some((min, max));
            }
            props = self.pattern_props()?;
            self.expect(&Tok::RBracket, "]")?;
        }
        self.expect(&Tok::Minus, "-")?;
        let right = self.eat(&Tok::Gt);
        let dir = match (left, right) {
            (false, true) => Dir::Out,
            (true, false) => Dir::In,
            (false, false) => Dir::Both,
            (true, true) => {
                return Err(CypherError::syntax(
                    "a relationship cannot point both ways".to_string(),
                ))
            }
        };
        Ok(RelPat {
            var,
            types,
            props,
            dir,
            range,
        })
    }

    // ---- expressions ----

    pub fn expr(&mut self) -> Result<Expr, CypherError> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<Expr, CypherError> {
        let mut left = self.xor_expr()?;
        while self.eat_kw("OR") {
            let right = self.xor_expr()?;
            left = Expr::Binary(BinOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn xor_expr(&mut self) -> Result<Expr, CypherError> {
        let mut left = self.and_expr()?;
        while self.eat_kw("XOR") {
            let right = self.and_expr()?;
            left = Expr::Binary(BinOp::Xor, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr, CypherError> {
        let mut left = self.not_expr()?;
        while self.eat_kw("AND") {
            let right = self.not_expr()?;
            left = Expr::Binary(BinOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<Expr, CypherError> {
        if self.eat_kw("NOT") {
            let inner = self.not_expr()?;
            return Ok(Expr::Unary(UnOp::Not, Box::new(inner)));
        }
        self.comparison()
    }

    /// Comparisons chain: `a < b <= c` is `a < b AND b <= c`.
    fn comparison(&mut self) -> Result<Expr, CypherError> {
        let first = self.predicate()?;
        let mut left = first.clone();
        let mut chain: Option<Expr> = None;
        loop {
            let op = match self.peek() {
                Tok::Eq => BinOp::Eq,
                Tok::Ne => BinOp::Ne,
                Tok::Lt => BinOp::Lt,
                Tok::Le => BinOp::Le,
                Tok::Gt => BinOp::Gt,
                Tok::Ge => BinOp::Ge,
                _ => break,
            };
            self.bump();
            let right = self.predicate()?;
            let link = Expr::Binary(op, Box::new(left), Box::new(right.clone()));
            chain = Some(match chain {
                None => link,
                Some(prev) => Expr::Binary(BinOp::And, Box::new(prev), Box::new(link)),
            });
            left = right;
        }
        Ok(chain.unwrap_or(first))
    }

    fn predicate(&mut self) -> Result<Expr, CypherError> {
        let mut left = self.additive()?;
        loop {
            if self.is_kw("IS") {
                self.bump();
                let negated = self.eat_kw("NOT");
                self.expect_kw("NULL")?;
                left = Expr::IsNull(Box::new(left), negated);
            } else if self.eat_kw("IN") {
                let right = self.additive()?;
                left = Expr::Binary(BinOp::In, Box::new(left), Box::new(right));
            } else if self.is_kw("STARTS") && self.is_kw_at(1, "WITH") {
                self.bump();
                self.bump();
                let right = self.additive()?;
                left = Expr::Binary(BinOp::StartsWith, Box::new(left), Box::new(right));
            } else if self.is_kw("ENDS") && self.is_kw_at(1, "WITH") {
                self.bump();
                self.bump();
                let right = self.additive()?;
                left = Expr::Binary(BinOp::EndsWith, Box::new(left), Box::new(right));
            } else if self.eat_kw("CONTAINS") {
                let right = self.additive()?;
                left = Expr::Binary(BinOp::Contains, Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Expr, CypherError> {
        let mut left = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.bump();
            let right = self.multiplicative()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn multiplicative(&mut self) -> Result<Expr, CypherError> {
        let mut left = self.power()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Mod,
                _ => break,
            };
            self.bump();
            let right = self.power()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `^` is left-associative in openCypher: `2 ^ 3 ^ 2` is `(2 ^ 3) ^ 2`.
    fn power(&mut self) -> Result<Expr, CypherError> {
        let mut left = self.unary()?;
        while self.eat(&Tok::Caret) {
            let right = self.unary()?;
            left = Expr::Binary(BinOp::Pow, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, CypherError> {
        if self.eat(&Tok::Minus) {
            let inner = self.unary()?;
            return Ok(match inner {
                Expr::Lit(Val::Int(i)) => {
                    Expr::Lit(Val::Int(i.checked_neg().ok_or_else(|| {
                        CypherError::syntax("integer literal out of range")
                    })?))
                }
                Expr::Lit(Val::Float(f)) => Expr::Lit(Val::Float(-f)),
                other => Expr::Unary(UnOp::Neg, Box::new(other)),
            });
        }
        if self.eat(&Tok::Plus) {
            return self.unary();
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr, CypherError> {
        let mut e = self.atom()?;
        loop {
            if self.eat(&Tok::Dot) {
                let key = self.ident("a property key")?;
                e = Expr::Prop(Box::new(e), key);
            } else if self.at(&Tok::LBracket) {
                self.bump();
                if self.eat(&Tok::DotDot) {
                    let hi = if self.at(&Tok::RBracket) {
                        None
                    } else {
                        Some(Box::new(self.expr()?))
                    };
                    self.expect(&Tok::RBracket, "]")?;
                    e = Expr::Slice(Box::new(e), None, hi);
                    continue;
                }
                let idx = self.expr()?;
                if self.eat(&Tok::DotDot) {
                    let hi = if self.at(&Tok::RBracket) {
                        None
                    } else {
                        Some(Box::new(self.expr()?))
                    };
                    self.expect(&Tok::RBracket, "]")?;
                    e = Expr::Slice(Box::new(e), Some(Box::new(idx)), hi);
                    continue;
                }
                self.expect(&Tok::RBracket, "]")?;
                e = Expr::Index(Box::new(e), Box::new(idx));
            } else if self.at(&Tok::Colon) && matches!(e, Expr::Var(_)) {
                e = Expr::HasLabels(Box::new(e), self.labels()?);
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn map_literal(&mut self) -> Result<Expr, CypherError> {
        self.expect(&Tok::LBrace, "{")?;
        let mut entries = Vec::new();
        if !self.at(&Tok::RBrace) {
            loop {
                let key = match self.bump() {
                    Tok::Ident { name, .. } => name,
                    Tok::Str(s) => s,
                    _ => {
                        self.i -= 1;
                        return Err(self.unexpected("a map key"));
                    }
                };
                self.expect(&Tok::Colon, ":")?;
                entries.push((key, self.expr()?));
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(&Tok::RBrace, "}")?;
        Ok(Expr::Map(entries))
    }

    fn atom(&mut self) -> Result<Expr, CypherError> {
        match self.peek().clone() {
            Tok::Int(i) => {
                self.bump();
                Ok(Expr::Lit(Val::Int(i)))
            }
            Tok::Float(f) => {
                self.bump();
                Ok(Expr::Lit(Val::Float(f)))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Expr::Lit(Val::Str(s)))
            }
            Tok::Param(name) => {
                self.bump();
                Ok(Expr::Param(name))
            }
            Tok::LParen => {
                // A pattern predicate `(a)-[:T]->(b)` or a parenthesized expression.
                if let Some(part) = self.try_relationship_pattern() {
                    return Ok(Expr::Exists {
                        patterns: vec![part],
                        where_: None,
                        subquery: false,
                    });
                }
                self.bump();
                let e = self.expr()?;
                self.expect(&Tok::RParen, ")")?;
                Ok(e)
            }
            Tok::LBracket => {
                self.bump();
                // Pattern comprehension `[(a)-->(b) WHERE pred | proj]`.
                let starts_pattern = self.at(&Tok::LParen)
                    || (matches!(self.peek(), Tok::Ident { .. })
                        && self.peek_at(1) == &Tok::Eq
                        && self.peek_at(2) == &Tok::LParen);
                if starts_pattern {
                    let save = self.i;
                    if let Some(part) = self.try_relationship_pattern() {
                        if self.is_kw("WHERE") || self.at(&Tok::Pipe) {
                            let pred = if self.eat_kw("WHERE") {
                                Some(Box::new(self.expr()?))
                            } else {
                                None
                            };
                            self.expect(&Tok::Pipe, "| in a pattern comprehension")?;
                            let proj = Box::new(self.expr()?);
                            self.expect(&Tok::RBracket, "]")?;
                            return Ok(Expr::PatternComp {
                                part: Box::new(part),
                                pred,
                                proj,
                            });
                        }
                    }
                    self.i = save;
                }
                if matches!(self.peek(), Tok::Ident { .. }) && self.is_kw_at(1, "IN") {
                    let var = self.variable()?;
                    self.bump();
                    let list = Box::new(self.expr()?);
                    let pred = if self.eat_kw("WHERE") {
                        Some(Box::new(self.expr()?))
                    } else {
                        None
                    };
                    let proj = if self.eat(&Tok::Pipe) {
                        Some(Box::new(self.expr()?))
                    } else {
                        None
                    };
                    self.expect(&Tok::RBracket, "]")?;
                    return Ok(Expr::ListComp {
                        var,
                        list,
                        pred,
                        proj,
                    });
                }
                let mut items = Vec::new();
                if !self.at(&Tok::RBracket) {
                    loop {
                        items.push(self.expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RBracket, "]")?;
                Ok(Expr::List(items))
            }
            Tok::LBrace => self.map_literal(),
            Tok::Ident { name, quoted } => {
                if !quoted {
                    let upper = name.to_ascii_uppercase();
                    match upper.as_str() {
                        "TRUE" => {
                            self.bump();
                            return Ok(Expr::Lit(Val::Bool(true)));
                        }
                        "FALSE" => {
                            self.bump();
                            return Ok(Expr::Lit(Val::Bool(false)));
                        }
                        "NULL" => {
                            self.bump();
                            return Ok(Expr::Lit(Val::Null));
                        }
                        "CASE" => {
                            self.bump();
                            return self.case_expr();
                        }
                        "EXISTS" if self.peek_at(1) == &Tok::LBrace => {
                            self.bump();
                            self.bump();
                            let _ = self.eat_kw("MATCH");
                            let patterns = self.pattern()?;
                            let where_ = if self.eat_kw("WHERE") {
                                Some(Box::new(self.expr()?))
                            } else {
                                None
                            };
                            if !self.at(&Tok::RBrace) {
                                return Err(CypherError::unsupported(
                                    "EXISTS { subquery } with clauses other than MATCH ... WHERE",
                                ));
                            }
                            self.bump();
                            return Ok(Expr::Exists {
                                patterns,
                                where_,
                                subquery: true,
                            });
                        }
                        _ => {}
                    }
                }
                if self.peek_at(1) == &Tok::LParen {
                    let quant = match name.to_ascii_lowercase().as_str() {
                        "all" => Some(Quant::All),
                        "any" => Some(Quant::Any),
                        "none" => Some(Quant::None),
                        "single" => Some(Quant::Single),
                        _ => None,
                    };
                    if let Some(kind) = quant {
                        if matches!(self.peek_at(2), Tok::Ident { .. }) && self.is_kw_at(3, "IN") {
                            self.bump();
                            self.bump();
                            let var = self.variable()?;
                            self.bump();
                            let list = Box::new(self.expr()?);
                            self.expect_kw("WHERE")?;
                            let pred = Box::new(self.expr()?);
                            self.expect(&Tok::RParen, ")")?;
                            return Ok(Expr::Quantifier {
                                kind,
                                var,
                                list,
                                pred,
                            });
                        }
                    }
                    return self.function_call();
                }
                if self.peek_at(1) == &Tok::Dot
                    && matches!(self.peek_at(2), Tok::Ident { .. })
                    && self.peek_at(3) == &Tok::LParen
                {
                    // Namespaced function, e.g. eh.active(n).
                    return self.function_call();
                }
                Ok(Expr::Var(self.variable()?))
            }
            _ => Err(self.unexpected("an expression")),
        }
    }

    /// Speculatively parse a pattern with at least one relationship, restoring the
    /// position when the tokens are not one.
    fn try_relationship_pattern(&mut self) -> Option<PatternPart> {
        let save = self.i;
        match self.pattern_part() {
            Ok(part) if !part.chain.is_empty() => Some(part),
            _ => {
                self.i = save;
                None
            }
        }
    }

    fn case_expr(&mut self) -> Result<Expr, CypherError> {
        let operand = if self.is_kw("WHEN") {
            None
        } else {
            Some(Box::new(self.expr()?))
        };
        let mut whens = Vec::new();
        while self.eat_kw("WHEN") {
            let cond = self.expr()?;
            self.expect_kw("THEN")?;
            let then = self.expr()?;
            whens.push((cond, then));
        }
        if whens.is_empty() {
            return Err(self.unexpected("WHEN"));
        }
        let else_ = if self.eat_kw("ELSE") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect_kw("END")?;
        Ok(Expr::Case {
            operand,
            whens,
            else_,
        })
    }

    fn function_call(&mut self) -> Result<Expr, CypherError> {
        let mut name = self.ident("a function name")?;
        while self.eat(&Tok::Dot) {
            name.push('.');
            name.push_str(&self.ident("a function name")?);
        }
        let name = name.to_ascii_lowercase();
        self.expect(&Tok::LParen, "(")?;
        if name == "count" && self.at(&Tok::Star) {
            self.bump();
            self.expect(&Tok::RParen, ")")?;
            return Ok(Expr::CountStar);
        }
        let distinct = self.eat_kw("DISTINCT");
        let mut args = Vec::new();
        if !self.at(&Tok::RParen) {
            loop {
                args.push(self.expr()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(&Tok::RParen, ")")?;
        Ok(Expr::Func {
            name,
            distinct,
            args,
        })
    }
}
