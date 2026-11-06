use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{validate_id, EhError, PropValue};

/// Server role. Only `primary` can run in the MVP.
///
/// `replica` and `standby` are named so configuration can reject them. They are not
/// implemented. Stage 1 needs election and store-side fencing before either can start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Primary,
    Replica,
    Standby,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Replica => "replica",
            Self::Standby => "standby",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceIdentity {
    pub instance_id: String,
    pub domain: String,
    pub role: Role,
}

impl InstanceIdentity {
    pub fn primary(instance_id: impl Into<String>, domain: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
            domain: domain.into(),
            role: Role::Primary,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ElementKind {
    Node,
    Edge,
}

/// Filter for `active`. JSON `true`, `false`, or `"any"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ActiveFilter {
    #[default]
    Active,
    Inactive,
    Any,
}

impl<'de> Deserialize<'de> for ActiveFilter {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Null => Ok(Self::Active),
            serde_json::Value::Bool(true) => Ok(Self::Active),
            serde_json::Value::Bool(false) => Ok(Self::Inactive),
            serde_json::Value::String(s) if s == "any" => Ok(Self::Any),
            other => Err(serde::de::Error::custom(format!(
                "active must be true, false, or \"any\", got {other}"
            ))),
        }
    }
}

impl ActiveFilter {
    pub fn matches(self, active: bool) -> bool {
        match self {
            Self::Active => active,
            Self::Inactive => !active,
            Self::Any => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(default)]
    pub props: BTreeMap<String, PropValue>,
    pub first_seen: i64,
    pub last_seen: i64,
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub source: String,
    pub execution_id: String,
    pub version_created: u64,
    pub version_updated: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub id: String,
    pub src: String,
    pub dst: String,
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(default)]
    pub props: BTreeMap<String, PropValue>,
    pub first_seen: i64,
    pub last_seen: i64,
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub source: String,
    pub execution_id: String,
    pub version_created: u64,
    pub version_updated: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeInput {
    pub id: String,
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(default)]
    pub props: BTreeMap<String, PropValue>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub first_seen: Option<i64>,
    #[serde(default)]
    pub last_seen: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeInput {
    #[serde(default)]
    pub id: Option<String>,
    pub src: String,
    pub dst: String,
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(default)]
    pub props: BTreeMap<String, PropValue>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub first_seen: Option<i64>,
    #[serde(default)]
    pub last_seen: Option<i64>,
}

impl NodeInput {
    pub fn validate(&self) -> Result<(), EhError> {
        validate_id(&self.id)?;
        if self.type_name.is_empty() || self.type_name.len() > 128 {
            return Err(EhError::invalid("node type must be 1..=128 characters"));
        }
        Ok(())
    }
}

impl EdgeInput {
    pub fn validate(&self) -> Result<(), EhError> {
        if let Some(id) = &self.id {
            validate_id(id)?;
        }
        validate_id(&self.src)?;
        validate_id(&self.dst)?;
        if self.type_name.is_empty() || self.type_name.len() > 128 {
            return Err(EhError::invalid("edge type must be 1..=128 characters"));
        }
        Ok(())
    }
}

/// A batch the API accepts. One batch becomes at most one graph version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WriteRequest {
    pub namespace: String,
    #[serde(default)]
    pub nodes: Vec<NodeInput>,
    #[serde(default)]
    pub edges: Vec<EdgeInput>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    pub source: String,
    pub execution_id: String,
    /// Observation time for elements that do not carry their own `last_seen`.
    /// Zero means "use the server clock".
    #[serde(default)]
    pub ts_ms: i64,
}

/// One durable log record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrameBody {
    Commit(ChangeBatch),
    /// Remembers an idempotency key whose batch did not change state.
    Idempotency(IdempotencyNote),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IdempotencyNote {
    pub namespace: String,
    pub key: String,
    pub body_hash: String,
    /// The `graph_version` the key is bound to. Not incremented.
    pub graph_version: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeBatch {
    pub namespace: String,
    pub graph_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub body_hash: String,
    pub source: String,
    pub execution_id: String,
    pub ts_ms: i64,
    pub ops: Vec<StoredOp>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StoredOp {
    UpsertNode { node: Node },
    UpsertEdge { edge: Edge },
    DeactivateNode { id: String, ts_ms: i64 },
    DeactivateEdge { id: String, ts_ms: i64 },
}

/// Content that, when equal, does not bump `graph_version`.
///
/// `last_seen` and `execution_id` are included only when the caller set `last_seen`
/// explicitly on the input. See `content_changed` in the store.
pub fn same_node_content(existing: &Node, incoming: &Node) -> bool {
    existing.type_name == incoming.type_name
        && existing.props == incoming.props
        && existing.origin == incoming.origin
        && existing.source == incoming.source
        && existing.active
        && existing.last_seen == incoming.last_seen
}

pub fn same_edge_content(existing: &Edge, incoming: &Edge) -> bool {
    existing.src == incoming.src
        && existing.dst == incoming.dst
        && existing.type_name == incoming.type_name
        && existing.props == incoming.props
        && existing.origin == incoming.origin
        && existing.source == incoming.source
        && existing.active
        && existing.last_seen == incoming.last_seen
}
