//! One resident graph: Arrow record batches plus CSR adjacency.

use std::collections::{BTreeMap, HashMap, VecDeque};

use ehgraph_core::{ActiveFilter, Edge, EhError, Node, PropPredicate};
use ehgraph_store::{batch_memory_size, edges_to_batch, nodes_to_batch, MaterializedGraph};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct SessionGraph {
    pub version: u64,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub node_batch: arrow_array::RecordBatch,
    pub edge_batch: arrow_array::RecordBatch,
    node_index: HashMap<String, u32>,
    edge_index: HashMap<String, u32>,
    out_offsets: Vec<u32>,
    out_edges: Vec<u32>,
    in_offsets: Vec<u32>,
    in_edges: Vec<u32>,
    pub resident_bytes: u64,
}

impl SessionGraph {
    pub fn from_materialized(graph: &MaterializedGraph) -> Result<Self, EhError> {
        let nodes = graph.node_list();
        let edges = graph.edge_list();
        let node_batch = nodes_to_batch(&nodes)?;
        let edge_batch = edges_to_batch(&edges)?;
        let mut node_index = HashMap::with_capacity(nodes.len());
        for (ordinal, node) in nodes.iter().enumerate() {
            node_index.insert(node.id.clone(), ordinal as u32);
        }
        let mut edge_index = HashMap::with_capacity(edges.len());
        for (ordinal, edge) in edges.iter().enumerate() {
            edge_index.insert(edge.id.clone(), ordinal as u32);
        }
        let mut out_lists = vec![Vec::new(); nodes.len()];
        let mut in_lists = vec![Vec::new(); nodes.len()];
        for (ordinal, edge) in edges.iter().enumerate() {
            if !edge.active {
                continue;
            }
            let Some(&src) = node_index.get(&edge.src) else {
                continue;
            };
            let Some(&dst) = node_index.get(&edge.dst) else {
                continue;
            };
            if !nodes[src as usize].active || !nodes[dst as usize].active {
                continue;
            }
            out_lists[src as usize].push(ordinal as u32);
            in_lists[dst as usize].push(ordinal as u32);
        }
        let (out_offsets, out_edges) = csr(&out_lists);
        let (in_offsets, in_edges) = csr(&in_lists);
        let arrow_bytes = batch_memory_size(&node_batch) + batch_memory_size(&edge_batch);
        let id_bytes = nodes.iter().map(|n| n.id.len() as u64).sum::<u64>()
            + edges.iter().map(|e| e.id.len() as u64).sum::<u64>();
        let csr_bytes = ((out_edges.len() + in_edges.len() + out_offsets.len() + in_offsets.len())
            * std::mem::size_of::<u32>()) as u64;
        let resident_bytes = arrow_bytes + id_bytes + csr_bytes + 64;
        Ok(Self {
            version: graph.version,
            nodes,
            edges,
            node_batch,
            edge_batch,
            node_index,
            edge_index,
            out_offsets,
            out_edges,
            in_offsets,
            in_edges,
            resident_bytes,
        })
    }

    pub fn node(&self, id: &str) -> Option<&Node> {
        self.node_index
            .get(id)
            .map(|ord| &self.nodes[*ord as usize])
    }

    pub fn edge(&self, id: &str) -> Option<&Edge> {
        self.edge_index
            .get(id)
            .map(|ord| &self.edges[*ord as usize])
    }

    pub fn query(&self, query: &NodeQuery) -> Page<Node> {
        let mut matched: Vec<&Node> = self
            .nodes
            .iter()
            .filter(|node| query.matches(node))
            .collect();
        matched.sort_by(|a, b| a.id.cmp(&b.id));
        page_after(matched, query.cursor.as_deref(), query.limit, |node| {
            &node.id
        })
        .map(Node::clone)
    }

    pub fn neighbors(&self, query: &NeighborQuery) -> Result<Page<Edge>, EhError> {
        let Some(ordinal) = self.node_index.get(&query.node_id) else {
            return Err(EhError::not_found(format!(
                "node {} not found",
                query.node_id
            )));
        };
        if !self.nodes[*ordinal as usize].active && query.active_only {
            return Err(EhError::not_found(format!(
                "node {} is inactive",
                query.node_id
            )));
        }
        let mut edges = Vec::new();
        if matches!(query.direction, Direction::Out | Direction::Both) {
            edges.extend(self.csr_edges(&self.out_offsets, &self.out_edges, *ordinal));
        }
        if matches!(query.direction, Direction::In | Direction::Both) {
            edges.extend(self.csr_edges(&self.in_offsets, &self.in_edges, *ordinal));
        }
        if let Some(type_name) = &query.edge_type {
            edges.retain(|edge| &edge.type_name == type_name);
        }
        edges.sort_by(|a, b| a.id.cmp(&b.id));
        edges.dedup_by(|a, b| a.id == b.id);
        Ok(
            page_after(edges, query.cursor.as_deref(), query.limit, |edge| &edge.id)
                .map(Edge::clone),
        )
    }

    pub fn subgraph(&self, query: &SubgraphQuery) -> Result<Subgraph, EhError> {
        for seed in &query.seeds {
            match self.node(seed) {
                Some(node) if node.active => {}
                Some(_) => return Err(EhError::invalid(format!("seed {seed} is inactive"))),
                None => return Err(EhError::not_found(format!("seed {seed} not found"))),
            }
        }
        let mut nodes = Vec::new();
        let mut seen = HashMap::new();
        let mut frontier: VecDeque<u32> = VecDeque::new();
        for seed in &query.seeds {
            let ord = self.node_index[seed];
            if seen.insert(ord, 0).is_none() {
                nodes.push(self.nodes[ord as usize].clone());
                frontier.push_back(ord);
            }
        }
        let mut edges = Vec::new();
        let mut truncated = false;
        while let Some(ord) = frontier.pop_front() {
            let depth = seen[&ord];
            if depth >= query.k {
                continue;
            }
            for edge_ord in self.csr_ords(&self.out_offsets, &self.out_edges, ord) {
                let edge = &self.edges[edge_ord as usize];
                if let Some(types) = &query.edge_types {
                    if !types.iter().any(|t| t == &edge.type_name) {
                        continue;
                    }
                }
                let Some(&dst) = self.node_index.get(&edge.dst) else {
                    continue;
                };
                if !self.nodes[dst as usize].active {
                    continue;
                }
                if !seen.contains_key(&dst) && nodes.len() >= query.max_nodes {
                    truncated = true;
                    continue;
                }
                edges.push(edge.clone());
                if seen.insert(dst, depth + 1).is_none() {
                    nodes.push(self.nodes[dst as usize].clone());
                    frontier.push_back(dst);
                }
            }
        }
        Ok(Subgraph {
            nodes,
            edges,
            truncated,
        })
    }

    pub fn path(&self, query: &PathQuery) -> Result<PathResult, EhError> {
        let src = self
            .node_index
            .get(&query.src)
            .copied()
            .ok_or_else(|| EhError::not_found(format!("node {} not found", query.src)))?;
        let dst = self
            .node_index
            .get(&query.dst)
            .copied()
            .ok_or_else(|| EhError::not_found(format!("node {} not found", query.dst)))?;
        if !self.nodes[src as usize].active || !self.nodes[dst as usize].active {
            return Err(EhError::not_found(
                "path endpoints must be active nodes".to_string(),
            ));
        }
        if src == dst {
            return Ok(PathResult {
                found: true,
                nodes: vec![query.src.clone()],
                edges: vec![],
            });
        }
        let mut prev: HashMap<u32, (u32, u32)> = HashMap::new();
        let mut queue = VecDeque::from([src]);
        let mut seen = HashMap::from([(src, 0u32)]);
        let mut found = false;
        while let Some(ord) = queue.pop_front() {
            let depth = seen[&ord];
            if depth >= query.max_hops {
                continue;
            }
            for edge_ord in self.csr_ords(&self.out_offsets, &self.out_edges, ord) {
                let edge = &self.edges[edge_ord as usize];
                if let Some(types) = &query.edge_types {
                    if !types.iter().any(|t| t == &edge.type_name) {
                        continue;
                    }
                }
                let Some(&next) = self.node_index.get(&edge.dst) else {
                    continue;
                };
                if seen.contains_key(&next) || !self.nodes[next as usize].active {
                    continue;
                }
                prev.insert(next, (ord, edge_ord));
                seen.insert(next, depth + 1);
                if next == dst {
                    found = true;
                    break;
                }
                queue.push_back(next);
            }
            if found {
                break;
            }
        }
        if !found {
            return Ok(PathResult {
                found: false,
                nodes: vec![],
                edges: vec![],
            });
        }
        let mut node_ords = vec![dst];
        let mut edge_ords = Vec::new();
        let mut cursor = dst;
        while cursor != src {
            let (prev_node, edge_ord) = prev[&cursor];
            edge_ords.push(edge_ord);
            node_ords.push(prev_node);
            cursor = prev_node;
        }
        node_ords.reverse();
        edge_ords.reverse();
        Ok(PathResult {
            found: true,
            nodes: node_ords
                .into_iter()
                .map(|ord| self.nodes[ord as usize].id.clone())
                .collect(),
            edges: edge_ords
                .into_iter()
                .map(|ord| self.edges[ord as usize].id.clone())
                .collect(),
        })
    }

    fn csr_ords(&self, offsets: &[u32], edges: &[u32], node: u32) -> Vec<u32> {
        let start = offsets[node as usize] as usize;
        let end = offsets[node as usize + 1] as usize;
        edges[start..end].to_vec()
    }

    fn csr_edges(&self, offsets: &[u32], edges: &[u32], node: u32) -> Vec<&Edge> {
        self.csr_ords(offsets, edges, node)
            .into_iter()
            .map(|ord| &self.edges[ord as usize])
            .collect()
    }
}

fn csr(lists: &[Vec<u32>]) -> (Vec<u32>, Vec<u32>) {
    let mut offsets = Vec::with_capacity(lists.len() + 1);
    let mut edges = Vec::new();
    offsets.push(0);
    for list in lists {
        edges.extend(list);
        offsets.push(edges.len() as u32);
    }
    (offsets, edges)
}

#[derive(Debug, Clone)]
pub struct NodeQuery {
    pub type_name: Option<String>,
    pub origin: Option<String>,
    pub props: BTreeMap<String, PropPredicate>,
    pub active: ActiveFilter,
    pub limit: usize,
    pub cursor: Option<String>,
}

impl NodeQuery {
    fn matches(&self, node: &Node) -> bool {
        if !self.active.matches(node.active) {
            return false;
        }
        if let Some(type_name) = &self.type_name {
            if &node.type_name != type_name {
                return false;
            }
        }
        if let Some(origin) = &self.origin {
            if node.origin.as_deref() != Some(origin.as_str()) {
                return false;
            }
        }
        self.props
            .iter()
            .all(|(key, pred)| node.props.get(key).is_some_and(|value| pred.matches(value)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
    Both,
}

#[derive(Debug, Clone)]
pub struct NeighborQuery {
    pub node_id: String,
    pub edge_type: Option<String>,
    pub direction: Direction,
    pub limit: usize,
    pub cursor: Option<String>,
    pub active_only: bool,
}

#[derive(Debug, Clone)]
pub struct SubgraphQuery {
    pub seeds: Vec<String>,
    pub k: u32,
    pub edge_types: Option<Vec<String>>,
    pub max_nodes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Subgraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct PathQuery {
    pub src: String,
    pub dst: String,
    pub edge_types: Option<Vec<String>>,
    pub max_hops: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct PathResult {
    pub found: bool,
    pub nodes: Vec<String>,
    pub edges: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub truncated: bool,
    pub next_cursor: Option<String>,
}

impl<T> Page<T> {
    fn map<U>(self, f: impl Fn(T) -> U) -> Page<U> {
        Page {
            items: self.items.into_iter().map(f).collect(),
            truncated: self.truncated,
            next_cursor: self.next_cursor,
        }
    }
}

fn page_after<T>(
    items: Vec<T>,
    cursor: Option<&str>,
    limit: usize,
    id_of: impl Fn(&T) -> &str,
) -> Page<T> {
    let limit = limit.clamp(1, 1000);
    let start = match cursor {
        Some(cursor) => items
            .iter()
            .position(|item| id_of(item) > cursor)
            .unwrap_or(items.len()),
        None => 0,
    };
    let rest = items.len().saturating_sub(start);
    let take = rest.min(limit);
    let truncated = rest > limit;
    let slice: Vec<T> = items.into_iter().skip(start).take(take).collect();
    let next_cursor = if truncated {
        slice.last().map(|item| id_of(item).to_string())
    } else {
        None
    };
    Page {
        items: slice,
        truncated,
        next_cursor,
    }
}
