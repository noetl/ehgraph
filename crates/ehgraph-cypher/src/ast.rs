//! Syntax tree of the supported openCypher subset.

use crate::value::Val;

#[derive(Debug, Clone)]
pub struct Query {
    pub clauses: Vec<Clause>,
}

impl Query {
    /// True when any clause changes the graph.
    pub fn is_updating(&self) -> bool {
        self.clauses.iter().any(|c| {
            matches!(
                c,
                Clause::Create(_)
                    | Clause::Merge { .. }
                    | Clause::Set(_)
                    | Clause::Remove(_)
                    | Clause::Delete { .. }
            )
        })
    }
}

#[derive(Debug, Clone)]
pub enum Clause {
    Match {
        optional: bool,
        patterns: Vec<PatternPart>,
        where_: Option<Expr>,
    },
    Unwind {
        expr: Expr,
        alias: String,
    },
    With(Projection),
    Return(Projection),
    Create(Vec<PatternPart>),
    Merge {
        part: PatternPart,
        on_create: Vec<SetItem>,
        on_match: Vec<SetItem>,
    },
    Set(Vec<SetItem>),
    Remove(Vec<RemoveItem>),
    Delete {
        detach: bool,
        exprs: Vec<Expr>,
    },
}

#[derive(Debug, Clone)]
pub struct Projection {
    pub distinct: bool,
    /// `RETURN *` / `WITH *`: every bound variable, then `items`.
    pub star: bool,
    pub items: Vec<(Expr, String)>,
    pub order: Vec<(Expr, bool)>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
    /// Only for `WITH ... WHERE`.
    pub where_: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct PatternPart {
    pub path_var: Option<String>,
    /// `shortestPath(...)` (false) or `allShortestPaths(...)` (true).
    pub shortest: Option<bool>,
    pub start: NodePat,
    pub chain: Vec<(RelPat, NodePat)>,
}

#[derive(Debug, Clone)]
pub struct NodePat {
    pub var: Option<String>,
    pub labels: Vec<String>,
    pub props: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// `-[]->`
    Out,
    /// `<-[]-`
    In,
    /// `-[]-`
    Both,
}

#[derive(Debug, Clone)]
pub struct RelPat {
    pub var: Option<String>,
    pub types: Vec<String>,
    pub props: Option<Expr>,
    pub dir: Dir,
    /// Variable length `*min..max`; `None` is a single hop.
    pub range: Option<(u32, Option<u32>)>,
}

#[derive(Debug, Clone)]
pub enum SetItem {
    Prop {
        var: String,
        key: String,
        value: Expr,
    },
    Replace {
        var: String,
        value: Expr,
    },
    Merge {
        var: String,
        value: Expr,
    },
    Labels {
        var: String,
        labels: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub enum RemoveItem {
    Prop { var: String, key: String },
    Labels { var: String, labels: Vec<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
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
    Pow,
    In,
    StartsWith,
    EndsWith,
    Contains,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Not,
    Neg,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Lit(Val),
    Param(String),
    Var(String),
    Prop(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Slice(Box<Expr>, Option<Box<Expr>>, Option<Box<Expr>>),
    List(Vec<Expr>),
    Map(Vec<(String, Expr)>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    IsNull(Box<Expr>, bool),
    HasLabels(Box<Expr>, Vec<String>),
    Func {
        name: String,
        distinct: bool,
        args: Vec<Expr>,
    },
    CountStar,
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        else_: Option<Box<Expr>>,
    },
    /// `[x IN list WHERE pred | proj]`
    ListComp {
        var: String,
        list: Box<Expr>,
        pred: Option<Box<Expr>>,
        proj: Option<Box<Expr>>,
    },
    /// `all/any/none/single(x IN list WHERE pred)`
    Quantifier {
        kind: Quant,
        var: String,
        list: Box<Expr>,
        pred: Box<Expr>,
    },
    /// Slot of a computed aggregate, after projection planning.
    AggRef(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quant {
    All,
    Any,
    None,
    Single,
}

pub const AGGREGATES: [&str; 6] = ["count", "collect", "sum", "avg", "min", "max"];

impl Expr {
    pub fn contains_aggregate(&self) -> bool {
        match self {
            Expr::CountStar => true,
            Expr::Func { name, args, .. } => {
                AGGREGATES.contains(&name.as_str()) || args.iter().any(Expr::contains_aggregate)
            }
            Expr::Prop(e, _) | Expr::Unary(_, e) | Expr::IsNull(e, _) | Expr::HasLabels(e, _) => {
                e.contains_aggregate()
            }
            Expr::Index(a, b) | Expr::Binary(_, a, b) => {
                a.contains_aggregate() || b.contains_aggregate()
            }
            Expr::Slice(a, b, c) => {
                a.contains_aggregate()
                    || b.as_ref().is_some_and(|e| e.contains_aggregate())
                    || c.as_ref().is_some_and(|e| e.contains_aggregate())
            }
            Expr::List(items) => items.iter().any(Expr::contains_aggregate),
            Expr::Map(items) => items.iter().any(|(_, e)| e.contains_aggregate()),
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                operand.as_ref().is_some_and(|e| e.contains_aggregate())
                    || whens
                        .iter()
                        .any(|(a, b)| a.contains_aggregate() || b.contains_aggregate())
                    || else_.as_ref().is_some_and(|e| e.contains_aggregate())
            }
            Expr::ListComp {
                list, pred, proj, ..
            } => {
                list.contains_aggregate()
                    || pred.as_ref().is_some_and(|e| e.contains_aggregate())
                    || proj.as_ref().is_some_and(|e| e.contains_aggregate())
            }
            Expr::Quantifier { list, pred, .. } => {
                list.contains_aggregate() || pred.contains_aggregate()
            }
            Expr::Lit(_) | Expr::Param(_) | Expr::Var(_) | Expr::AggRef(_) => false,
        }
    }
}
