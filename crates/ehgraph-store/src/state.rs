use std::collections::{BTreeMap, BTreeSet};

use ehgraph_core::{
    same_edge_content, same_node_content, ChangeBatch, Edge, EdgeInput, EhError, FrameBody,
    IdempotencyNote, Node, NodeInput, SchemaConfig, Scope, StoredOp, WriteRequest,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Idempo {
    pub version: u64,
    pub hash: String,
}

/// Derived view of one namespace. Rebuildable from the log and full snapshots.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MaterializedGraph {
    pub version: u64,
    pub nodes: BTreeMap<String, Node>,
    pub edges: BTreeMap<String, Edge>,
    out_adj: BTreeMap<String, Vec<String>>,
    in_adj: BTreeMap<String, Vec<String>>,
}

impl MaterializedGraph {
    pub fn from_elements(version: u64, nodes: Vec<Node>, edges: Vec<Edge>) -> Self {
        let mut graph = Self {
            version,
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            out_adj: BTreeMap::new(),
            in_adj: BTreeMap::new(),
        };
        for node in nodes {
            graph.nodes.insert(node.id.clone(), node);
        }
        for edge in edges {
            graph.link_edge(&edge);
            graph.edges.insert(edge.id.clone(), edge);
        }
        graph
    }

    pub fn node_list(&self) -> Vec<Node> {
        self.nodes.values().cloned().collect()
    }

    pub fn edge_list(&self) -> Vec<Edge> {
        self.edges.values().cloned().collect()
    }

    pub fn out_edges(&self, id: &str) -> Vec<&Edge> {
        self.edge_refs(&self.out_adj, id)
    }

    pub fn in_edges(&self, id: &str) -> Vec<&Edge> {
        self.edge_refs(&self.in_adj, id)
    }

    fn edge_refs<'a>(&'a self, index: &BTreeMap<String, Vec<String>>, id: &str) -> Vec<&'a Edge> {
        index
            .get(id)
            .map(|ids| ids.iter().filter_map(|eid| self.edges.get(eid)).collect())
            .unwrap_or_default()
    }

    pub fn apply_batch(&mut self, batch: &ChangeBatch) -> Result<(), EhError> {
        if batch.graph_version != self.version + 1 {
            return Err(EhError::unavailable(format!(
                "change log version gap: have {}, next record is {}",
                self.version, batch.graph_version
            )));
        }
        for op in &batch.ops {
            self.apply_op(op, batch.graph_version)?;
        }
        self.version = batch.graph_version;
        Ok(())
    }

    fn apply_op(&mut self, op: &StoredOp, version: u64) -> Result<(), EhError> {
        match op {
            StoredOp::UpsertNode { node } => {
                if self.edges.contains_key(&node.id) {
                    return Err(EhError::unavailable(format!(
                        "node id {} collides with an edge id in the log",
                        node.id
                    )));
                }
                self.nodes.insert(node.id.clone(), node.clone());
            }
            StoredOp::UpsertEdge { edge } => {
                if self.nodes.contains_key(&edge.id) {
                    return Err(EhError::unavailable(format!(
                        "edge id {} collides with a node id in the log",
                        edge.id
                    )));
                }
                if let Some(previous) = self.edges.get(&edge.id).cloned() {
                    self.unlink_edge(&previous);
                }
                self.link_edge(edge);
                self.edges.insert(edge.id.clone(), edge.clone());
            }
            StoredOp::DeactivateNode { id, ts_ms } => {
                let node = self
                    .nodes
                    .get_mut(id)
                    .ok_or_else(|| EhError::unavailable(format!("deactivate missing node {id}")))?;
                if node.active {
                    node.active = false;
                    node.last_seen = *ts_ms;
                    node.version_updated = version;
                }
            }
            StoredOp::DeactivateEdge { id, ts_ms } => {
                let Some(previous) = self.edges.get(id).cloned() else {
                    return Err(EhError::unavailable(format!(
                        "deactivate missing edge {id}"
                    )));
                };
                if previous.active {
                    self.unlink_edge(&previous);
                    let edge = self.edges.get_mut(id).expect("edge exists");
                    edge.active = false;
                    edge.last_seen = *ts_ms;
                    edge.version_updated = version;
                }
            }
        }
        Ok(())
    }

    fn link_edge(&mut self, edge: &Edge) {
        if !edge.active {
            return;
        }
        self.out_adj
            .entry(edge.src.clone())
            .or_default()
            .push(edge.id.clone());
        self.in_adj
            .entry(edge.dst.clone())
            .or_default()
            .push(edge.id.clone());
    }

    fn unlink_edge(&mut self, edge: &Edge) {
        if let Some(list) = self.out_adj.get_mut(&edge.src) {
            list.retain(|id| id != &edge.id);
        }
        if let Some(list) = self.in_adj.get_mut(&edge.dst) {
            list.retain(|id| id != &edge.id);
        }
    }

    pub fn apply_scope(&self, scope: &Scope) -> Result<Self, EhError> {
        match scope {
            Scope::Full => Ok(self.clone()),
            Scope::Filter { type_name, props } => {
                let nodes: Vec<Node> = self
                    .nodes
                    .values()
                    .filter(|node| node.active && node_matches(node, type_name, props))
                    .cloned()
                    .collect();
                let keep: BTreeSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
                let edges: Vec<Edge> = self
                    .edges
                    .values()
                    .filter(|edge| {
                        edge.active
                            && keep.contains(edge.src.as_str())
                            && keep.contains(edge.dst.as_str())
                    })
                    .cloned()
                    .collect();
                Ok(Self::from_elements(self.version, nodes, edges))
            }
            Scope::Khop {
                seeds,
                k,
                edge_types,
            } => self.khop(seeds, *k, edge_types),
            Scope::Snapshot { .. } => Err(EhError::invalid(
                "snapshot scopes are resolved by the store before filtering",
            )),
        }
    }

    fn khop(
        &self,
        seeds: &[String],
        k: u32,
        edge_types: &Option<Vec<String>>,
    ) -> Result<Self, EhError> {
        let mut seen_nodes: BTreeSet<String> = BTreeSet::new();
        for seed in seeds {
            match self.nodes.get(seed) {
                Some(node) if node.active => {
                    seen_nodes.insert(seed.clone());
                }
                Some(_) => {
                    return Err(EhError::invalid(format!("seed {seed} is inactive")));
                }
                None => return Err(EhError::not_found(format!("seed {seed} not found"))),
            }
        }
        let mut frontier: Vec<String> = seeds.to_vec();
        let mut seen_edges: BTreeSet<String> = BTreeSet::new();
        for _ in 0..k {
            let mut next = Vec::new();
            for node_id in frontier {
                for edge in self.out_edges(&node_id) {
                    if !edge.active {
                        continue;
                    }
                    if let Some(types) = edge_types {
                        if !types.iter().any(|t| t == &edge.type_name) {
                            continue;
                        }
                    }
                    match self.nodes.get(&edge.dst) {
                        Some(dst) if dst.active => {}
                        _ => continue,
                    }
                    seen_edges.insert(edge.id.clone());
                    if seen_nodes.insert(edge.dst.clone()) {
                        next.push(edge.dst.clone());
                    }
                }
            }
            frontier = next;
        }
        let nodes = seen_nodes
            .iter()
            .filter_map(|id| self.nodes.get(id).cloned())
            .collect();
        let edges = seen_edges
            .iter()
            .filter_map(|id| self.edges.get(id).cloned())
            .collect();
        Ok(Self::from_elements(self.version, nodes, edges))
    }
}

fn node_matches(
    node: &Node,
    type_name: &Option<String>,
    props: &BTreeMap<String, ehgraph_core::PropPredicate>,
) -> bool {
    if let Some(expected) = type_name {
        if &node.type_name != expected {
            return false;
        }
    }
    props
        .iter()
        .all(|(key, pred)| node.props.get(key).is_some_and(|value| pred.matches(value)))
}

pub fn remember(keys: &mut BTreeMap<String, Idempo>, key: &str, version: u64, hash: &str) {
    keys.insert(
        key.to_string(),
        Idempo {
            version,
            hash: hash.to_string(),
        },
    );
}

pub fn absorb_frame(
    graph: &mut MaterializedGraph,
    keys: &mut BTreeMap<String, Idempo>,
    frame: &FrameBody,
    apply_commits: bool,
) -> Result<(), EhError> {
    match frame {
        FrameBody::Commit(batch) => {
            if let Some(key) = &batch.idempotency_key {
                remember(keys, key, batch.graph_version, &batch.body_hash);
            }
            if apply_commits {
                graph.apply_batch(batch)?;
            }
        }
        FrameBody::Idempotency(IdempotencyNote {
            key,
            body_hash,
            graph_version,
            ..
        }) => {
            remember(keys, key, *graph_version, body_hash);
        }
    }
    Ok(())
}

/// Build the committed images for an upsert. An empty `ops` list means the batch is a no-op.
pub fn plan_upsert(
    graph: &MaterializedGraph,
    request: &WriteRequest,
    schema: &SchemaConfig,
) -> Result<Vec<StoredOp>, EhError> {
    if request.nodes.is_empty() && request.edges.is_empty() {
        return Err(EhError::invalid(
            "upsert requires at least one node or edge",
        ));
    }
    if request.source.is_empty() || request.execution_id.is_empty() {
        return Err(EhError::invalid("source and execution_id are required"));
    }
    let mut scratch = graph.clone();
    let mut ops = Vec::new();
    for input in &request.nodes {
        if let Some(op) = plan_node(&scratch, input, request, schema)? {
            scratch.apply_op(&op, scratch.version)?;
            ops.push(op);
        }
    }
    for input in &request.edges {
        if let Some(op) = plan_edge(&scratch, input, request, schema)? {
            scratch.apply_op(&op, scratch.version)?;
            ops.push(op);
        }
    }
    Ok(ops)
}

fn plan_node(
    graph: &MaterializedGraph,
    input: &NodeInput,
    request: &WriteRequest,
    schema: &SchemaConfig,
) -> Result<Option<StoredOp>, EhError> {
    input.validate()?;
    schema.check_node(input)?;
    if graph.edges.contains_key(&input.id) {
        return Err(EhError::invalid(format!(
            "node id {} is already an edge id",
            input.id
        )));
    }
    let candidate = match graph.nodes.get(&input.id) {
        Some(existing) => {
            let content_changed = existing.type_name != input.type_name
                || existing.props != input.props
                || existing.origin != input.origin
                || existing.source != request.source
                || !existing.active;
            let last_seen = if content_changed {
                input.last_seen.unwrap_or(request.ts_ms)
            } else {
                input.last_seen.unwrap_or(existing.last_seen)
            };
            Node {
                id: input.id.clone(),
                type_name: input.type_name.clone(),
                props: input.props.clone(),
                first_seen: existing.first_seen,
                last_seen,
                active: true,
                origin: input.origin.clone(),
                source: request.source.clone(),
                execution_id: request.execution_id.clone(),
                version_created: existing.version_created,
                version_updated: existing.version_updated,
            }
        }
        None => {
            let seen = input
                .last_seen
                .or(input.first_seen)
                .unwrap_or(request.ts_ms);
            Node {
                id: input.id.clone(),
                type_name: input.type_name.clone(),
                props: input.props.clone(),
                first_seen: input.first_seen.unwrap_or(seen),
                last_seen: input.last_seen.unwrap_or(seen),
                active: true,
                origin: input.origin.clone(),
                source: request.source.clone(),
                execution_id: request.execution_id.clone(),
                version_created: 0,
                version_updated: 0,
            }
        }
    };
    if let Some(existing) = graph.nodes.get(&input.id) {
        if same_node_content(existing, &candidate) {
            return Ok(None);
        }
    }
    Ok(Some(StoredOp::UpsertNode { node: candidate }))
}

fn plan_edge(
    graph: &MaterializedGraph,
    input: &EdgeInput,
    request: &WriteRequest,
    schema: &SchemaConfig,
) -> Result<Option<StoredOp>, EhError> {
    input.validate()?;
    schema.check_edge(input)?;
    require_active_node(graph, &input.src)?;
    require_active_node(graph, &input.dst)?;
    let id = match &input.id {
        Some(id) => id.clone(),
        None => ehgraph_core::default_edge_id(
            &input.src,
            &input.type_name,
            &input.dst,
            input.origin.as_deref(),
        ),
    };
    if graph.nodes.contains_key(&id) {
        return Err(EhError::invalid(format!(
            "edge id {id} is already a node id"
        )));
    }
    let candidate = match graph.edges.get(&id) {
        Some(existing) => {
            let content_changed = existing.src != input.src
                || existing.dst != input.dst
                || existing.type_name != input.type_name
                || existing.props != input.props
                || existing.origin != input.origin
                || existing.source != request.source
                || !existing.active;
            let last_seen = if content_changed {
                input.last_seen.unwrap_or(request.ts_ms)
            } else {
                input.last_seen.unwrap_or(existing.last_seen)
            };
            Edge {
                id,
                src: input.src.clone(),
                dst: input.dst.clone(),
                type_name: input.type_name.clone(),
                props: input.props.clone(),
                first_seen: existing.first_seen,
                last_seen,
                active: true,
                origin: input.origin.clone(),
                source: request.source.clone(),
                execution_id: request.execution_id.clone(),
                version_created: existing.version_created,
                version_updated: existing.version_updated,
            }
        }
        None => {
            let seen = input
                .last_seen
                .or(input.first_seen)
                .unwrap_or(request.ts_ms);
            Edge {
                id,
                src: input.src.clone(),
                dst: input.dst.clone(),
                type_name: input.type_name.clone(),
                props: input.props.clone(),
                first_seen: input.first_seen.unwrap_or(seen),
                last_seen: input.last_seen.unwrap_or(seen),
                active: true,
                origin: input.origin.clone(),
                source: request.source.clone(),
                execution_id: request.execution_id.clone(),
                version_created: 0,
                version_updated: 0,
            }
        }
    };
    if let Some(existing) = graph.edges.get(&candidate.id) {
        if same_edge_content(existing, &candidate) {
            return Ok(None);
        }
    }
    Ok(Some(StoredOp::UpsertEdge { edge: candidate }))
}

fn require_active_node(graph: &MaterializedGraph, id: &str) -> Result<(), EhError> {
    match graph.nodes.get(id) {
        Some(node) if node.active => Ok(()),
        Some(_) => Err(EhError::invalid(format!("edge endpoint {id} is inactive"))),
        None => Err(EhError::invalid(format!(
            "edge endpoint {id} does not exist"
        ))),
    }
}

pub fn stamp_ops(ops: &mut [StoredOp], version: u64) {
    for op in ops {
        match op {
            StoredOp::UpsertNode { node } => {
                if node.version_created == 0 {
                    node.version_created = version;
                }
                node.version_updated = version;
            }
            StoredOp::UpsertEdge { edge } => {
                if edge.version_created == 0 {
                    edge.version_created = version;
                }
                edge.version_updated = version;
            }
            StoredOp::DeactivateNode { .. } | StoredOp::DeactivateEdge { .. } => {}
        }
    }
}

pub fn plan_deactivate(
    graph: &MaterializedGraph,
    ids: &[String],
    origin: Option<&str>,
    source: Option<&str>,
    not_seen_since_ms: Option<i64>,
    ts_ms: i64,
) -> Result<Vec<StoredOp>, EhError> {
    let by_ids = !ids.is_empty();
    let by_selector = source.is_some() && not_seen_since_ms.is_some();
    if by_ids == by_selector {
        return Err(EhError::invalid(
            "deactivate requires either ids or {source, not_seen_since}, not both and not neither",
        ));
    }
    let mut ops = Vec::new();
    if by_ids {
        for id in ids {
            match (graph.nodes.get(id), graph.edges.get(id)) {
                (Some(node), None) => {
                    if node.active {
                        ops.push(StoredOp::DeactivateNode {
                            id: id.clone(),
                            ts_ms,
                        });
                    }
                }
                (None, Some(edge)) => {
                    if edge.active {
                        ops.push(StoredOp::DeactivateEdge {
                            id: id.clone(),
                            ts_ms,
                        });
                    }
                }
                (None, None) => {
                    return Err(EhError::not_found(format!("element {id} not found")));
                }
                (Some(_), Some(_)) => {
                    return Err(EhError::invalid(format!(
                        "id {id} is both a node and an edge"
                    )));
                }
            }
        }
        return Ok(ops);
    }
    let source = source.expect("selector source");
    let cutoff = not_seen_since_ms.expect("selector cutoff");
    for node in graph.nodes.values() {
        if node.active
            && node.source == source
            && origin.is_none_or(|value| node.origin.as_deref() == Some(value))
            && node.last_seen < cutoff
        {
            ops.push(StoredOp::DeactivateNode {
                id: node.id.clone(),
                ts_ms,
            });
        }
    }
    for edge in graph.edges.values() {
        if edge.active
            && edge.source == source
            && origin.is_none_or(|value| edge.origin.as_deref() == Some(value))
            && edge.last_seen < cutoff
        {
            ops.push(StoredOp::DeactivateEdge {
                id: edge.id.clone(),
                ts_ms,
            });
        }
    }
    Ok(ops)
}
