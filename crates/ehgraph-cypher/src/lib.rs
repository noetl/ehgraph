//! openCypher (GQL-aligned) query subset over the ehgraph labeled property graph.
//!
//! A query is parsed, then executed against one read view of a namespace (the head
//! view, a pinned version, or a session graph). Updating clauses do not touch the
//! store: they edit a copy-on-write overlay of that view, and execution returns the
//! net [`Changes`] — full-image node/edge upserts plus soft deactivations — which the
//! caller commits with the ordinary `graph.upsert` / `graph.deactivate` machinery.
//! That keeps every invariant of the existing write path (validation, schema,
//! idempotent no-op on identical content, versioning) in one place.
//!
//! Mapping (see `docs/wiki/Cypher.md`):
//! - node label  ↔ node `type` (ehgraph nodes carry exactly one label)
//! - relationship type ↔ edge `type`
//! - property `id` ↔ the element id (reserved; never stored in `props`)
//! - `DELETE` ↔ soft deactivation (history and versions are kept)
//! - the namespace is the graph; it is chosen by the request, not the query.

mod ast;
mod exec;
mod lexer;
mod parser;
mod value;

pub use ast::Query;
pub use exec::{
    execute, execute_in, Changes, ExecOptions, Footprint, Out, Outcome, Overlay, Stats, TxState,
};
pub use parser::parse;
pub use value::Val;

use ehgraph_core::{EhError, ErrorCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Syntax,
    Unsupported,
    Type,
    Semantic,
    Constraint,
    Limit,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Unsupported => "unsupported",
            Self::Type => "type",
            Self::Semantic => "semantic",
            Self::Constraint => "constraint",
            Self::Limit => "limit",
        }
    }

    /// The closest Neo4j status code, for the Query-API-compatible route.
    pub fn neo4j_code(self) -> &'static str {
        match self {
            Self::Syntax | Self::Unsupported => "Neo.ClientError.Statement.SyntaxError",
            Self::Type => "Neo.ClientError.Statement.TypeError",
            Self::Semantic => "Neo.ClientError.Statement.SemanticError",
            Self::Constraint => "Neo.ClientError.Schema.ConstraintValidationFailed",
            Self::Limit => "Neo.ClientError.Statement.ArgumentError",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CypherError {
    pub kind: ErrorKind,
    pub message: String,
}

impl CypherError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    pub fn syntax(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Syntax, message)
    }
    pub fn unsupported(what: &str) -> Self {
        Self::new(
            ErrorKind::Unsupported,
            format!("{what} is not supported by ehgraph's openCypher subset yet (see docs/wiki/Cypher.md)"),
        )
    }
    pub fn type_error(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Type, message)
    }
    pub fn semantic(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Semantic, message)
    }
    pub fn constraint(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Constraint, message)
    }
    pub fn limit(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Limit, message)
    }
}

impl std::fmt::Display for CypherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cypher {} error: {}", self.kind.as_str(), self.message)
    }
}

impl std::error::Error for CypherError {}

impl From<CypherError> for EhError {
    fn from(err: CypherError) -> Self {
        let code = match err.kind {
            ErrorKind::Constraint => ErrorCode::Conflict,
            _ => ErrorCode::InvalidArgument,
        };
        EhError::new(code, err.to_string())
    }
}

/// Recover the Cypher error kind from an error message produced by `From<CypherError>`.
pub fn kind_of_message(message: &str) -> Option<ErrorKind> {
    let rest = message.strip_prefix("cypher ")?;
    let kind = rest.split(' ').next()?;
    [
        ErrorKind::Syntax,
        ErrorKind::Unsupported,
        ErrorKind::Type,
        ErrorKind::Semantic,
        ErrorKind::Constraint,
        ErrorKind::Limit,
    ]
    .into_iter()
    .find(|k| k.as_str() == kind)
}
