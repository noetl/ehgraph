//! Runtime values and their openCypher comparison / ordering rules.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use ehgraph_core::PropValue;
use serde_json::Value as Json;

use crate::CypherError;

#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// ehgraph timestamp property (unix millis).
    Ts(i64),
    List(Vec<Val>),
    Map(BTreeMap<String, Val>),
    /// A node, by ehgraph element id.
    Node(String),
    /// A relationship, by ehgraph element id.
    Rel(String),
    Path {
        nodes: Vec<String>,
        rels: Vec<String>,
    },
}

impl Val {
    pub fn type_name(&self) -> &'static str {
        match self {
            Val::Null => "NULL",
            Val::Bool(_) => "BOOLEAN",
            Val::Int(_) => "INTEGER",
            Val::Float(_) => "FLOAT",
            Val::Str(_) => "STRING",
            Val::Ts(_) => "TIMESTAMP",
            Val::List(_) => "LIST",
            Val::Map(_) => "MAP",
            Val::Node(_) => "NODE",
            Val::Rel(_) => "RELATIONSHIP",
            Val::Path { .. } => "PATH",
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Val::Null)
    }

    pub fn from_prop(p: &PropValue) -> Val {
        match p {
            PropValue::String(s) => Val::Str(s.clone()),
            PropValue::Int(i) => Val::Int(*i),
            PropValue::Float(f) => Val::Float(*f),
            PropValue::Bool(b) => Val::Bool(*b),
            PropValue::Timestamp(t) => Val::Ts(*t),
            PropValue::List(items) => Val::List(items.iter().map(Val::from_prop).collect()),
            PropValue::Map(m) => Val::Map(
                m.iter()
                    .map(|(k, v)| (k.clone(), Val::from_prop(v)))
                    .collect(),
            ),
        }
    }

    /// A storable property value. `Ok(None)` is null, which removes the property.
    pub fn to_prop(&self) -> Result<Option<PropValue>, CypherError> {
        Ok(Some(match self {
            Val::Null => return Ok(None),
            Val::Bool(b) => PropValue::Bool(*b),
            Val::Int(i) => PropValue::Int(*i),
            Val::Float(f) => {
                if !f.is_finite() {
                    return Err(CypherError::type_error("property floats must be finite"));
                }
                PropValue::Float(*f)
            }
            Val::Str(s) => PropValue::String(s.clone()),
            Val::Ts(t) => PropValue::Timestamp(*t),
            Val::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item.to_prop()? {
                        Some(p) => out.push(p),
                        None => {
                            return Err(CypherError::type_error(
                                "lists stored as properties cannot contain null",
                            ))
                        }
                    }
                }
                PropValue::List(out)
            }
            Val::Map(m) => {
                let mut out = BTreeMap::new();
                for (k, v) in m {
                    if let Some(p) = v.to_prop()? {
                        out.insert(k.clone(), p);
                    }
                }
                PropValue::Map(out)
            }
            Val::Node(_) | Val::Rel(_) | Val::Path { .. } => {
                return Err(CypherError::type_error(format!(
                    "a {} cannot be stored as a property",
                    self.type_name()
                )))
            }
        }))
    }

    /// Parameters arrive as JSON. `{"$timestamp": ...}` becomes a timestamp.
    pub fn from_json(j: &Json) -> Val {
        match j {
            Json::Null => Val::Null,
            Json::Bool(b) => Val::Bool(*b),
            Json::Number(n) => n
                .as_i64()
                .map(Val::Int)
                .unwrap_or_else(|| Val::Float(n.as_f64().unwrap_or(f64::NAN))),
            Json::String(s) => Val::Str(s.clone()),
            Json::Array(items) => Val::List(items.iter().map(Val::from_json).collect()),
            Json::Object(m) => {
                if m.len() == 1 && m.contains_key("$timestamp") {
                    if let Ok(PropValue::Timestamp(t)) = PropValue::from_json(j) {
                        return Val::Ts(t);
                    }
                }
                Val::Map(
                    m.iter()
                        .map(|(k, v)| (k.clone(), Val::from_json(v)))
                        .collect(),
                )
            }
        }
    }

    pub fn truthy(&self) -> Result<Option<bool>, CypherError> {
        match self {
            Val::Null => Ok(None),
            Val::Bool(b) => Ok(Some(*b)),
            other => Err(CypherError::type_error(format!(
                "expected a boolean, got {}",
                other.type_name()
            ))),
        }
    }

    fn num(&self) -> Option<f64> {
        match self {
            Val::Int(i) => Some(*i as f64),
            Val::Float(f) => Some(*f),
            _ => None,
        }
    }
}

/// openCypher equality: `None` is null (unknown).
pub fn equals(a: &Val, b: &Val) -> Option<bool> {
    match (a, b) {
        (Val::Null, _) | (_, Val::Null) => None,
        (Val::Int(x), Val::Int(y)) => Some(x == y),
        (Val::Int(_) | Val::Float(_), Val::Int(_) | Val::Float(_)) => Some(a.num() == b.num()),
        (Val::List(x), Val::List(y)) => {
            if x.len() != y.len() {
                return Some(false);
            }
            let mut unknown = false;
            for (l, r) in x.iter().zip(y) {
                match equals(l, r) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            if unknown {
                None
            } else {
                Some(true)
            }
        }
        (Val::Map(x), Val::Map(y)) => {
            if x.len() != y.len() || x.keys().ne(y.keys()) {
                return Some(false);
            }
            let mut unknown = false;
            for (l, r) in x.values().zip(y.values()) {
                match equals(l, r) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            if unknown {
                None
            } else {
                Some(true)
            }
        }
        _ => {
            if std::mem::discriminant(a) == std::mem::discriminant(b) {
                Some(a == b)
            } else {
                Some(false)
            }
        }
    }
}

/// openCypher comparability for `<`, `<=`, `>`, `>=`. `None` when incomparable.
pub fn compare(a: &Val, b: &Val) -> Option<Ordering> {
    match (a, b) {
        (Val::Int(x), Val::Int(y)) => Some(x.cmp(y)),
        (Val::Int(_) | Val::Float(_), Val::Int(_) | Val::Float(_)) => {
            a.num()?.partial_cmp(&b.num()?)
        }
        (Val::Str(x), Val::Str(y)) => Some(x.cmp(y)),
        (Val::Bool(x), Val::Bool(y)) => Some(x.cmp(y)),
        (Val::Ts(x), Val::Ts(y)) => Some(x.cmp(y)),
        (Val::List(x), Val::List(y)) => {
            for (l, r) in x.iter().zip(y) {
                match compare(l, r)? {
                    Ordering::Equal => continue,
                    other => return Some(other),
                }
            }
            Some(x.len().cmp(&y.len()))
        }
        _ => None,
    }
}

/// Total order used by ORDER BY (openCypher orderability; nulls sort last).
pub fn order(a: &Val, b: &Val) -> Ordering {
    fn rank(v: &Val) -> u8 {
        match v {
            Val::Map(_) => 0,
            Val::Node(_) => 1,
            Val::Rel(_) => 2,
            Val::List(_) => 3,
            Val::Path { .. } => 4,
            Val::Ts(_) => 5,
            Val::Str(_) => 6,
            Val::Bool(_) => 7,
            Val::Int(_) | Val::Float(_) => 8,
            Val::Null => 9,
        }
    }
    let (ra, rb) = (rank(a), rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Val::Int(_) | Val::Float(_), _) => {
            let (x, y) = (a.num().unwrap_or(0.0), b.num().unwrap_or(0.0));
            if x.is_nan() || y.is_nan() {
                return x.is_nan().cmp(&y.is_nan());
            }
            x.partial_cmp(&y).unwrap_or(Ordering::Equal)
        }
        (Val::List(x), Val::List(y)) => {
            for (l, r) in x.iter().zip(y) {
                let o = order(l, r);
                if o != Ordering::Equal {
                    return o;
                }
            }
            x.len().cmp(&y.len())
        }
        (Val::Node(x), Val::Node(y)) | (Val::Rel(x), Val::Rel(y)) => x.cmp(y),
        _ => compare(a, b).unwrap_or_else(|| key(a).cmp(&key(b))),
    }
}

/// Canonical text key for grouping and DISTINCT (equivalence, where null = null).
pub fn key(v: &Val) -> String {
    match v {
        Val::Null => "n".into(),
        Val::Bool(b) => format!("b{b}"),
        Val::Int(i) => format!("f{}", *i as f64),
        Val::Float(f) => format!("f{f}"),
        Val::Str(s) => format!("s{}:{s}", s.len()),
        Val::Ts(t) => format!("t{t}"),
        Val::List(items) => {
            let inner: Vec<String> = items.iter().map(key).collect();
            format!("l[{}]", inner.join(","))
        }
        Val::Map(m) => {
            let inner: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{}:{k}={}", k.len(), key(v)))
                .collect();
            format!("m{{{}}}", inner.join(","))
        }
        Val::Node(id) => format!("N{}:{id}", id.len()),
        Val::Rel(id) => format!("R{}:{id}", id.len()),
        Val::Path { nodes, rels } => format!("P{nodes:?}{rels:?}"),
    }
}
