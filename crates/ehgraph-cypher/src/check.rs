//! Static (compile-time) variable checks, run after parsing.
//!
//! openCypher rejects these before execution; without them a query that binds no
//! rows (an empty graph) would silently succeed. Checked here:
//!
//! - undefined variables (`RETURN x` with nothing named `x` in scope);
//! - a variable reused as a different kind of element (node vs relationship vs
//!   path vs plain value), e.g. `MATCH ()-[r]->() MATCH (r)`;
//! - a variable rebound where openCypher forbids it: `CREATE`/`MERGE` of an already
//!   bound relationship or of a bound node with new labels/properties, `MERGE (a)` of
//!   a bound node, and a path variable that is already in scope.
//!
//! Variables whose kind cannot be known statically (projected expressions,
//! parameters, list elements) are never reported as conflicts.

use std::collections::BTreeMap;

use crate::ast::*;
use crate::value::Val;
use crate::CypherError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Node,
    Rel,
    /// A variable-length relationship variable (a list of relationships).
    RelList,
    Path,
    /// Certainly not a graph element (a literal).
    Value,
    /// A list literal: a value, or the relationships of a variable-length pattern.
    List,
    Unknown,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Node => "a node",
            Kind::Rel => "a relationship",
            Kind::RelList => "a list of relationships",
            Kind::Path => "a path",
            Kind::Value | Kind::List | Kind::Unknown => "a value",
        }
    }
}

type Scope = BTreeMap<String, Kind>;

/// Check `query`, and expand `RETURN *` / `WITH *` into the variables in scope
/// (sorted by name), so the result has its columns even when it has no rows.
pub(crate) fn check(query: &mut Query) -> Result<(), CypherError> {
    single(&mut query.clauses)?;
    for (_, part) in &mut query.unions {
        single(part)?;
    }
    let columns = |clauses: &[Clause]| match clauses.last() {
        Some(Clause::Return(p)) if !p.star => {
            Some(p.items.iter().map(|(_, a)| a.clone()).collect::<Vec<_>>())
        }
        _ => None,
    };
    for (_, part) in &query.unions {
        if columns(&query.clauses) != columns(part) || columns(part).is_none() {
            return Err(CypherError::semantic(
                "all queries in a UNION must return the same column names",
            ));
        }
    }
    Ok(())
}

fn single(clauses: &mut [Clause]) -> Result<(), CypherError> {
    let mut scope = Scope::new();
    for clause in clauses.iter_mut() {
        match clause {
            Clause::Match {
                patterns, where_, ..
            } => {
                for part in patterns {
                    bind_match(part, &mut scope)?;
                }
                if let Some(w) = where_ {
                    where_expr(w, &scope)?;
                }
            }
            Clause::Unwind { expr: e, alias } => {
                expr(e, &scope)?;
                scope.insert(alias.clone(), Kind::Unknown);
            }
            Clause::With(p) | Clause::Return(p) => {
                let next = projection(p, &scope)?;
                if p.star {
                    let mut items: Vec<(Expr, String)> = scope
                        .keys()
                        .filter(|k| !p.items.iter().any(|(_, a)| a == *k))
                        .map(|k| (Expr::Var(k.clone()), k.clone()))
                        .collect();
                    items.append(&mut p.items);
                    p.items = items;
                    p.star = false;
                }
                scope = next;
            }
            Clause::Create(parts) => {
                for part in parts {
                    bind_create(part, &mut scope)?;
                }
            }
            Clause::Merge {
                part,
                on_create,
                on_match,
            } => {
                bind_create(part, &mut scope)?;
                for item in on_create.iter().chain(on_match.iter()) {
                    set_item(item, &scope)?;
                }
            }
            Clause::Set(items) => {
                for item in items {
                    set_item(item, &scope)?;
                }
            }
            Clause::Remove(items) => {
                for item in items {
                    let var = match item {
                        RemoveItem::Prop { var, .. } | RemoveItem::Labels { var, .. } => var,
                    };
                    defined(var, &scope)?;
                }
            }
            Clause::Delete { exprs, .. } => {
                for e in exprs {
                    expr(e, &scope)?;
                }
            }
        }
    }
    Ok(())
}

fn undefined(var: &str) -> CypherError {
    CypherError::semantic(format!("variable `{var}` not defined"))
}

fn conflict(var: &str, was: Kind, now: Kind) -> CypherError {
    CypherError::semantic(format!(
        "variable `{var}` is {} and cannot be used as {}",
        was.name(),
        now.name()
    ))
}

fn already_bound(var: &str) -> CypherError {
    CypherError::semantic(format!("variable `{var}` is already bound"))
}

fn defined(var: &str, scope: &Scope) -> Result<(), CypherError> {
    if scope.contains_key(var) {
        Ok(())
    } else {
        Err(undefined(var))
    }
}

/// Bind `var` as `kind`, reporting a conflict with a known different kind.
fn bind(var: &str, kind: Kind, scope: &mut Scope) -> Result<(), CypherError> {
    match scope.get(var) {
        Some(Kind::List) if kind == Kind::RelList => Ok(()),
        Some(&was) if was != kind && was != Kind::Unknown => Err(conflict(var, was, kind)),
        Some(Kind::Unknown) => Ok(()),
        Some(_) => Ok(()),
        None => {
            scope.insert(var.to_string(), kind);
            Ok(())
        }
    }
}

fn rel_kind(rp: &RelPat) -> Kind {
    if rp.range.is_some() {
        Kind::RelList
    } else {
        Kind::Rel
    }
}

fn bind_match(part: &PatternPart, scope: &mut Scope) -> Result<(), CypherError> {
    if let Some(pv) = &part.path_var {
        if scope.contains_key(pv) {
            return Err(already_bound(pv));
        }
    }
    let nodes = std::iter::once(&part.start).chain(part.chain.iter().map(|(_, n)| n));
    for np in nodes {
        if let Some(v) = &np.var {
            bind(v, Kind::Node, scope)?;
        }
        if let Some(p) = &np.props {
            expr(p, scope)?;
        }
    }
    for (rp, _) in &part.chain {
        if let Some(v) = &rp.var {
            bind(v, rel_kind(rp), scope)?;
        }
        if let Some(p) = &rp.props {
            expr(p, scope)?;
        }
    }
    if let Some(pv) = &part.path_var {
        bind(pv, Kind::Path, scope)?;
    }
    Ok(())
}

fn bind_create(part: &PatternPart, scope: &mut Scope) -> Result<(), CypherError> {
    if let Some(pv) = &part.path_var {
        if scope.contains_key(pv) {
            return Err(already_bound(pv));
        }
    }
    let single = part.chain.is_empty();
    let nodes = std::iter::once(&part.start).chain(part.chain.iter().map(|(_, n)| n));
    for np in nodes {
        if let Some(p) = &np.props {
            expr(p, scope)?;
        }
        if let Some(v) = &np.var {
            if let Some(&was) = scope.get(v) {
                if was != Kind::Node && was != Kind::Unknown {
                    return Err(conflict(v, was, Kind::Node));
                }
                if !np.labels.is_empty() || np.props.is_some() || single {
                    return Err(already_bound(v));
                }
            } else {
                scope.insert(v.clone(), Kind::Node);
            }
        }
    }
    for (rp, _) in &part.chain {
        if let Some(p) = &rp.props {
            expr(p, scope)?;
        }
        if let Some(v) = &rp.var {
            if scope.contains_key(v) {
                return Err(already_bound(v));
            }
            scope.insert(v.clone(), rel_kind(rp));
        }
    }
    if let Some(pv) = &part.path_var {
        scope.insert(pv.clone(), Kind::Path);
    }
    Ok(())
}

fn set_item(item: &SetItem, scope: &Scope) -> Result<(), CypherError> {
    match item {
        SetItem::Prop { var, value, .. }
        | SetItem::Replace { var, value }
        | SetItem::Merge { var, value } => {
            defined(var, scope)?;
            expr(value, scope)
        }
        SetItem::Labels { var, .. } => defined(var, scope),
    }
}

/// Check a WITH/RETURN projection; returns the scope after it.
/// Variables of an ORDER BY item after an aggregating or DISTINCT projection.
fn projected_only(
    e: &Expr,
    next: &Scope,
    both: &Scope,
    projected: &[String],
) -> Result<(), CypherError> {
    if projected.contains(&format!("{e:?}")) {
        return Ok(());
    }
    match e {
        Expr::Var(v) => defined(v, next),
        // Local variables: checked leniently.
        Expr::ListComp { .. } | Expr::Quantifier { .. } => vars(e, both),
        _ => {
            let mut res = Ok(());
            children(e, &mut |c| {
                if res.is_ok() {
                    res = projected_only(c, next, both, projected);
                }
            });
            res
        }
    }
}

fn pattern_names(part: &PatternPart) -> Vec<&String> {
    let nodes = std::iter::once(&part.start).chain(part.chain.iter().map(|(_, n)| n));
    nodes
        .filter_map(|n| n.var.as_ref())
        .chain(part.chain.iter().filter_map(|(r, _)| r.var.as_ref()))
        .chain(part.path_var.iter())
        .collect()
}

/// A bare pattern `(a)-->(b)` is a predicate: allowed only where a boolean is
/// expected (a WHERE, under AND/OR/XOR/NOT) or as the argument of `exists()`.
fn pattern_positions(e: &Expr, ok: bool) -> Result<(), CypherError> {
    match e {
        Expr::Exists {
            subquery: false, ..
        } if !ok => Err(CypherError::syntax(
            "a pattern expression is only allowed as a predicate (in WHERE or exists())",
        )),
        Expr::Exists { where_, .. } => where_.iter().try_for_each(|w| pattern_positions(w, true)),
        Expr::Binary(BinOp::And | BinOp::Or | BinOp::Xor, a, b) => {
            pattern_positions(a, ok)?;
            pattern_positions(b, ok)
        }
        Expr::Unary(UnOp::Not, a) => pattern_positions(a, ok),
        Expr::Func { name, args, .. }
            if name == "exists" && matches!(args.as_slice(), [Expr::Exists { .. }]) =>
        {
            pattern_positions(&args[0], true)
        }
        _ => {
            let mut res = Ok(());
            children(e, &mut |c| {
                if res.is_ok() {
                    res = pattern_positions(c, false);
                }
            });
            res
        }
    }
}

/// A WHERE predicate: pattern predicates allowed, aggregates not.
fn where_expr(e: &Expr, scope: &Scope) -> Result<(), CypherError> {
    pattern_positions(e, true)?;
    no_agg_vars(e, scope)
}

/// An expression outside a projection and outside WHERE.
fn expr(e: &Expr, scope: &Scope) -> Result<(), CypherError> {
    pattern_positions(e, false)?;
    no_agg_vars(e, scope)
}

/// No aggregates allowed; variables must be defined.
fn no_agg_vars(e: &Expr, scope: &Scope) -> Result<(), CypherError> {
    if e.contains_aggregate() {
        return Err(invalid_aggregation(
            "aggregate functions are only allowed in RETURN and WITH projections",
        ));
    }
    vars(e, scope)
}

fn invalid_aggregation(msg: &str) -> CypherError {
    CypherError::semantic(format!("invalid aggregation: {msg}"))
}

fn is_agg(e: &Expr) -> bool {
    matches!(e, Expr::CountStar)
        || matches!(e, Expr::Func { name, .. } if AGGREGATES.contains(&name.as_str()))
}

/// No aggregate inside another aggregate or inside a list comprehension/quantifier.
fn agg_structure(e: &Expr, inside: bool) -> Result<(), CypherError> {
    if is_agg(e) {
        if inside {
            return Err(invalid_aggregation(
                "aggregates cannot be nested or used inside list comprehensions",
            ));
        }
        if let Expr::Func { args, .. } = e {
            for a in args {
                agg_structure(a, true)?;
            }
        }
        return Ok(());
    }
    match e {
        // The list may be an aggregate (`[x IN collect(r) WHERE …]`); the
        // per-element predicate and projection may not.
        Expr::ListComp {
            list, pred, proj, ..
        } => {
            if pred
                .iter()
                .chain(proj.iter())
                .any(|x| x.contains_aggregate())
            {
                return Err(invalid_aggregation(
                    "aggregates cannot be used inside list comprehensions or quantifiers",
                ));
            }
            agg_structure(list, inside)
        }
        Expr::Quantifier { list, pred, .. } => {
            if pred.contains_aggregate() {
                return Err(invalid_aggregation(
                    "aggregates cannot be used inside list comprehensions or quantifiers",
                ));
            }
            agg_structure(list, inside)
        }
        _ => {
            // Walk direct children with the same rule.
            let mut res = Ok(());
            children(e, &mut |c| {
                if res.is_ok() {
                    res = agg_structure(c, inside);
                }
            });
            res
        }
    }
}

fn children<'e>(e: &'e Expr, f: &mut dyn FnMut(&'e Expr)) {
    match e {
        Expr::Lit(_) | Expr::Param(_) | Expr::Var(_) | Expr::CountStar | Expr::AggRef(_) => {}
        Expr::Prop(b, _) | Expr::Unary(_, b) | Expr::IsNull(b, _) | Expr::HasLabels(b, _) => f(b),
        Expr::Index(a, b) | Expr::Binary(_, a, b) => {
            f(a);
            f(b);
        }
        Expr::Slice(a, b, c) => {
            f(a);
            b.iter().chain(c.iter()).for_each(|x| f(x));
        }
        Expr::List(items) | Expr::Func { args: items, .. } => items.iter().for_each(|x| f(x)),
        Expr::Map(entries) => entries.iter().for_each(|(_, x)| f(x)),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            operand.iter().chain(else_.iter()).for_each(|x| f(x));
            for (a, b) in whens {
                f(a);
                f(b);
            }
        }
        Expr::ListComp {
            list, pred, proj, ..
        } => {
            f(list);
            pred.iter().chain(proj.iter()).for_each(|x| f(x));
        }
        Expr::Quantifier { list, pred, .. } => {
            f(list);
            f(pred);
        }
        Expr::Exists { where_, .. } => where_.iter().for_each(|x| f(x)),
        Expr::PatternComp { pred, proj, .. } => {
            pred.iter().for_each(|x| f(x));
            f(proj);
        }
    }
}

fn projection(p: &Projection, scope: &Scope) -> Result<Scope, CypherError> {
    if p.star && scope.is_empty() {
        return Err(CypherError::semantic(
            "RETURN * / WITH * with no variables in scope",
        ));
    }
    let mut next = if p.star { scope.clone() } else { Scope::new() };
    for e in p
        .items
        .iter()
        .map(|(e, _)| e)
        .chain(p.order.iter().map(|(e, _)| e))
    {
        agg_structure(e, false)?;
        pattern_positions(e, false)?;
    }
    let aggregating = p.items.iter().any(|(e, _)| e.contains_aggregate());
    if !aggregating && p.order.iter().any(|(e, _)| e.contains_aggregate()) {
        return Err(invalid_aggregation(
            "ORDER BY may use an aggregate only when the projection aggregates",
        ));
    }
    for (e, alias) in &p.items {
        vars(e, scope)?;
        let kind = match e {
            Expr::Var(v) => scope.get(v).copied().unwrap_or(Kind::Unknown),
            Expr::Lit(Val::Null) => Kind::Unknown,
            Expr::Lit(_) | Expr::Map(_) => Kind::Value,
            // A list may hold relationships (`[r1, r2] AS rs ... -[rs*]->`).
            Expr::List(_) => Kind::List,
            _ => Kind::Unknown,
        };
        next.insert(alias.clone(), kind);
    }
    // ORDER BY may use projected aliases and the variables in scope before the
    // projection; WHERE after WITH likewise. After an aggregating or DISTINCT
    // projection, ORDER BY sees only what was projected (an expression equal to a
    // projected one counts as projected).
    let mut both = scope.clone();
    both.extend(next.iter().map(|(k, v)| (k.clone(), *v)));
    let narrowed = p.distinct || p.items.iter().any(|(e, _)| e.contains_aggregate());
    let projected: Vec<String> = p.items.iter().map(|(e, _)| format!("{e:?}")).collect();
    for (e, _) in &p.order {
        if narrowed {
            projected_only(e, &next, &both, &projected)?;
        } else {
            vars(e, &both)?;
        }
    }
    if let Some(w) = &p.where_ {
        where_expr(w, &both)?;
    }
    for e in p.skip.iter().chain(p.limit.iter()) {
        expr(e, &Scope::new())?;
    }
    Ok(next)
}

fn vars(e: &Expr, scope: &Scope) -> Result<(), CypherError> {
    match e {
        Expr::Lit(_) | Expr::Param(_) | Expr::CountStar | Expr::AggRef(_) => Ok(()),
        Expr::Var(v) => defined(v, scope),
        Expr::Prop(b, _) | Expr::Unary(_, b) | Expr::IsNull(b, _) | Expr::HasLabels(b, _) => {
            vars(b, scope)
        }
        Expr::Index(a, b) | Expr::Binary(_, a, b) => {
            vars(a, scope)?;
            vars(b, scope)
        }
        Expr::Slice(a, b, c) => {
            vars(a, scope)?;
            for x in b.iter().chain(c.iter()) {
                vars(x, scope)?;
            }
            Ok(())
        }
        Expr::List(items) | Expr::Func { args: items, .. } => {
            items.iter().try_for_each(|x| vars(x, scope))
        }
        Expr::Map(entries) => entries.iter().try_for_each(|(_, x)| vars(x, scope)),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            for x in operand.iter().chain(else_.iter()) {
                vars(x, scope)?;
            }
            for (a, b) in whens {
                vars(a, scope)?;
                vars(b, scope)?;
            }
            Ok(())
        }
        Expr::ListComp {
            var,
            list,
            pred,
            proj,
        } => {
            vars(list, scope)?;
            let mut inner = scope.clone();
            inner.insert(var.clone(), Kind::Unknown);
            for x in pred.iter().chain(proj.iter()) {
                vars(x, &inner)?;
            }
            Ok(())
        }
        Expr::Quantifier {
            var, list, pred, ..
        } => {
            vars(list, scope)?;
            let mut inner = scope.clone();
            inner.insert(var.clone(), Kind::Unknown);
            vars(pred, &inner)
        }
        // Variables introduced inside an existential pattern or a pattern
        // comprehension are local to it.
        Expr::Exists {
            patterns,
            where_,
            subquery,
        } => {
            // A bare pattern predicate cannot introduce new variables.
            if !subquery {
                for part in patterns {
                    for v in pattern_names(part) {
                        defined(v, scope)?;
                    }
                }
            }
            let mut inner = scope.clone();
            for part in patterns {
                bind_match(part, &mut inner)?;
            }
            where_.iter().try_for_each(|w| where_expr(w, &inner))
        }
        Expr::PatternComp { part, pred, proj } => {
            let mut inner = scope.clone();
            bind_match(part, &mut inner)?;
            pred.iter().try_for_each(|w| expr(w, &inner))?;
            vars(proj, &inner)
        }
    }
}
