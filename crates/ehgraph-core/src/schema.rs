use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{EdgeInput, EhError, NodeInput};

/// Optional per-namespace schema.
///
/// The MVP is schemaless unless a schema file is present. When `strict` is set,
/// only declared types are accepted, and `required` properties must be present.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaConfig {
    #[serde(default)]
    pub strict: bool,
    #[serde(default)]
    pub nodes: BTreeMap<String, TypeRule>,
    #[serde(default)]
    pub edges: BTreeMap<String, TypeRule>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeRule {
    #[serde(default)]
    pub required: Vec<String>,
}

impl SchemaConfig {
    pub fn check_node(&self, node: &NodeInput) -> Result<(), EhError> {
        check_type(
            self.strict,
            &self.nodes,
            &node.type_name,
            &node.props,
            "node",
        )
    }

    pub fn check_edge(&self, edge: &EdgeInput) -> Result<(), EhError> {
        check_type(
            self.strict,
            &self.edges,
            &edge.type_name,
            &edge.props,
            "edge",
        )
    }
}

fn check_type(
    strict: bool,
    types: &BTreeMap<String, TypeRule>,
    type_name: &str,
    props: &BTreeMap<String, crate::PropValue>,
    kind: &str,
) -> Result<(), EhError> {
    match types.get(type_name) {
        Some(rule) => {
            for key in &rule.required {
                if !props.contains_key(key) {
                    return Err(EhError::invalid(format!(
                        "{kind} type {type_name} requires property {key}"
                    )));
                }
            }
            Ok(())
        }
        None if strict => Err(EhError::invalid(format!(
            "{kind} type {type_name} is not declared in the namespace schema"
        ))),
        None => Ok(()),
    }
}
