//! Shared types for the ehgraph topology database server.
//!
//! These types are the internal model of the server. They are not an embedding API.
//! Consumers reach ehgraph only over MCP and HTTP.

mod error;
mod ids;
mod model;
mod props;
mod schema;
mod scope;

pub use error::{EhError, ErrorBody, ErrorCode};
pub use ids::{canonical_hash, default_edge_id, validate_id, validate_namespace, validate_owner};
pub use model::{
    same_edge_content, same_node_content, ActiveFilter, ChangeBatch, Edge, EdgeInput, ElementKind,
    FrameBody, IdempotencyNote, InstanceIdentity, Node, NodeInput, Role, StoredOp, WriteRequest,
};
pub use props::{PropPredicate, PropValue};
pub use schema::{SchemaConfig, TypeRule};
pub use scope::Scope;

/// Unix epoch milliseconds.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Result envelope carried by every MCP tool and HTTP route.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Envelope {
    pub ok: bool,
    pub instance_id: String,
    pub domain: String,
    pub graph_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Envelope {
    pub fn success(
        identity: &InstanceIdentity,
        graph_version: Option<u64>,
        data: serde_json::Value,
    ) -> Self {
        Self {
            ok: true,
            instance_id: identity.instance_id.clone(),
            domain: identity.domain.clone(),
            graph_version,
            data: Some(data),
            truncated: None,
            next_cursor: None,
            error: None,
        }
    }

    pub fn failure(identity: &InstanceIdentity, err: &EhError) -> Self {
        Self {
            ok: false,
            instance_id: identity.instance_id.clone(),
            domain: identity.domain.clone(),
            graph_version: None,
            data: None,
            truncated: None,
            next_cursor: None,
            error: Some(err.to_body()),
        }
    }

    pub fn with_page(mut self, truncated: bool, next_cursor: Option<String>) -> Self {
        if truncated {
            self.truncated = Some(true);
            self.next_cursor = next_cursor;
        }
        self
    }
}
