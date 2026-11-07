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

// ---------------------------------------------------------------------------
// Resident-size estimates (register C11, noetl/ehgraph#6).
//
// The materialized graph is always resident: every element lives in a BTreeMap
// keyed by id, and every edge also has an entry in the out- and in-adjacency maps.
// These estimates are what the resident-graph budget charges. They are calibrated
// against a counting allocator in `ehgraph-store/tests/resident_bytes.rs`, which
// fails if they drift more than 25% from the measured heap.

// Measured shape (Rust std BTreeMap, B = 6, 64-bit):
// - A non-empty map allocates whole leaves of 11 slots, so a 2-entry props map costs
//   a full leaf (about 11 x (key + value) bytes). This dominates small elements.
// - A large map filled in key order keeps its leaves about half full, so an entry
//   costs about twice its key + value. Random-order inserts fill better; 2x is the
//   conservative figure.

const LEAF_SLOTS: u64 = 11;
const LEAF_HEADER: u64 = 16;
const LARGE_MAP_FACTOR: u64 = 2;

fn heap(s: &str) -> u64 {
    s.len() as u64
}

/// Bytes of the leaf (and internal) nodes of a small map with `n` entries of
/// `entry` bytes each.
fn small_map_nodes(n: u64, entry: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let leaves = if n <= LEAF_SLOTS {
        1
    } else {
        (2 * n).div_ceil(LEAF_SLOTS) + 1
    };
    leaves * (LEAF_SLOTS * entry + LEAF_HEADER)
}

fn props_bytes(props: &BTreeMap<String, PropValue>) -> u64 {
    let entry = (std::mem::size_of::<String>() + std::mem::size_of::<PropValue>()) as u64;
    small_map_nodes(props.len() as u64, entry)
        + props
            .iter()
            .map(|(k, v)| heap(k) + prop_value_heap(v))
            .sum::<u64>()
}

fn prop_value_heap(value: &PropValue) -> u64 {
    match value {
        PropValue::String(s) => heap(s),
        PropValue::List(items) => items
            .iter()
            .map(|v| std::mem::size_of::<PropValue>() as u64 + prop_value_heap(v))
            .sum(),
        PropValue::Map(map) => props_bytes(map),
        PropValue::Int(_) | PropValue::Float(_) | PropValue::Bool(_) | PropValue::Timestamp(_) => 0,
    }
}

fn opt_heap(s: &Option<String>) -> u64 {
    s.as_deref().map(heap).unwrap_or(0)
}

impl Node {
    /// Estimated resident bytes of this node in a materialized graph: its share of
    /// the id-keyed map, the id key, and every heap string and property.
    pub fn approx_bytes(&self) -> u64 {
        LARGE_MAP_FACTOR * (std::mem::size_of::<String>() + std::mem::size_of::<Node>()) as u64
            + 2 * heap(&self.id)
            + heap(&self.type_name)
            + opt_heap(&self.origin)
            + heap(&self.source)
            + heap(&self.execution_id)
            + props_bytes(&self.props)
    }
}

impl Edge {
    /// Estimated resident bytes of this edge: as for [`Node::approx_bytes`], plus its
    /// entries in the out- and in-adjacency indexes (the edge id in each endpoint's
    /// list, with `Vec` growth slack, and a share of the endpoint keys).
    pub fn approx_bytes(&self) -> u64 {
        let string = std::mem::size_of::<String>() as u64;
        let element = LARGE_MAP_FACTOR * (string + std::mem::size_of::<Edge>() as u64)
            + 2 * heap(&self.id)
            + heap(&self.src)
            + heap(&self.dst)
            + heap(&self.type_name)
            + opt_heap(&self.origin)
            + heap(&self.source)
            + heap(&self.execution_id)
            + props_bytes(&self.props);
        let ids = 2 * (string + heap(&self.id)) * 3 / 2;
        let endpoint_keys = 2 * (2 * string) + heap(&self.src) + heap(&self.dst);
        element + ids + endpoint_keys
    }
}
