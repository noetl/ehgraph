use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::PropPredicate;

/// What a session or a snapshot export contains.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    #[default]
    Full,
    Filter {
        #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
        type_name: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        props: BTreeMap<String, PropPredicate>,
    },
    Khop {
        seeds: Vec<String>,
        k: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        edge_types: Option<Vec<String>>,
    },
    Snapshot {
        snapshot_id: String,
    },
}

impl Scope {
    pub fn canonical(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}
