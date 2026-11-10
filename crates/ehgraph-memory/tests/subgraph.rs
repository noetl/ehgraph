//! Regression tests for `graph.subgraph` (noetl/ehgraph#20).
//!
//! A seed that is also the target of another seed's edge must keep depth 0, so its
//! own out-edges are still expanded. The bug overwrote its depth with 1 and, at
//! `k = 1`, silently dropped those edges with `truncated = false`.

use ehgraph_core::{Edge, Node};
use ehgraph_memory::{SessionGraph, SubgraphQuery};
use ehgraph_store::MaterializedGraph;

fn node(id: &str, type_name: &str) -> Node {
    Node {
        id: id.into(),
        type_name: type_name.into(),
        props: Default::default(),
        first_seen: 1,
        last_seen: 1,
        active: true,
        origin: None,
        source: "fixture".into(),
        execution_id: "exec".into(),
        version_created: 1,
        version_updated: 1,
    }
}

fn edge(src: &str, type_name: &str, dst: &str) -> Edge {
    Edge {
        id: format!("edge:{src}:{type_name}:{dst}:"),
        src: src.into(),
        dst: dst.into(),
        type_name: type_name.into(),
        props: Default::default(),
        first_seen: 1,
        last_seen: 1,
        active: true,
        origin: None,
        source: "fixture".into(),
        execution_id: "exec".into(),
        version_created: 1,
        version_updated: 1,
    }
}

/// The shape of the live qnetmon topology from the issue: APs uplink to a switch,
/// the switch uplinks to the gateway, the gateway has WAN and PART_OF edges, and APs
/// broadcast SSIDs that sit on networks.
fn topology() -> SessionGraph {
    let nodes = vec![
        node("ap:1", "AccessPoint"),
        node("ap:2", "AccessPoint"),
        node("ap:3", "AccessPoint"),
        node("sw:1", "Switch"),
        node("gw:1", "Gateway"),
        node("site:default", "Site"),
        node("internet:wan", "Internet"),
        node("ssid:1", "Ssid"),
        node("ssid:2", "Ssid"),
        node("net:1", "Network"),
        node("net:2", "Network"),
    ];
    let edges = vec![
        edge("ap:1", "UPLINK", "sw:1"),
        edge("ap:2", "UPLINK", "sw:1"),
        edge("ap:3", "UPLINK", "sw:1"),
        edge("ap:1", "BROADCASTS", "ssid:1"),
        edge("ap:2", "BROADCASTS", "ssid:2"),
        edge("sw:1", "UPLINK", "gw:1"),
        edge("gw:1", "PART_OF", "site:default"),
        edge("gw:1", "WAN", "internet:wan"),
        edge("ssid:1", "ON_NETWORK", "net:1"),
        edge("ssid:2", "ON_NETWORK", "net:2"),
    ];
    SessionGraph::from_materialized(&MaterializedGraph::from_elements(1, nodes, edges)).unwrap()
}

fn all_ids(graph: &SessionGraph) -> Vec<String> {
    let mut ids: Vec<String> = graph.nodes.iter().map(|n| n.id.clone()).collect();
    ids.sort();
    ids
}

#[test]
fn every_seed_at_k1_returns_every_active_edge() {
    let graph = topology();
    let active_edges = graph.edges.iter().filter(|e| e.active).count();
    // Seed order matters for the bug: a seed processed early must not demote a later
    // seed. Try sorted, reversed, and topology order.
    let mut orders = vec![all_ids(&graph)];
    orders.push(orders[0].iter().rev().cloned().collect());
    orders.push(graph.nodes.iter().map(|n| n.id.clone()).collect());
    for seeds in orders {
        let sub = graph
            .subgraph(&SubgraphQuery {
                seeds: seeds.clone(),
                k: 1,
                edge_types: None,
                max_nodes: 1000,
            })
            .unwrap();
        assert!(!sub.truncated);
        assert_eq!(sub.nodes.len(), graph.nodes.len());
        assert_eq!(
            sub.edges.len(),
            active_edges,
            "seeds {seeds:?} returned {:?}",
            sub.edges.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
    }
}

#[test]
fn seed_reached_from_another_seed_keeps_depth_zero() {
    let graph = topology();
    // ap:1 -> sw:1 -> gw:1. Both ap:1 and sw:1 are seeds; at k = 1 the result must
    // include sw:1's own out-edge to gw:1, even though ap:1 reaches sw:1 first.
    let sub = graph
        .subgraph(&SubgraphQuery {
            seeds: vec!["ap:1".into(), "sw:1".into()],
            k: 1,
            edge_types: Some(vec!["UPLINK".into()]),
            max_nodes: 1000,
        })
        .unwrap();
    let mut ids: Vec<&str> = sub.edges.iter().map(|e| e.id.as_str()).collect();
    ids.sort();
    assert_eq!(
        ids,
        vec!["edge:ap:1:UPLINK:sw:1:", "edge:sw:1:UPLINK:gw:1:"]
    );
}

#[test]
fn k2_from_one_seed_is_unchanged() {
    let graph = topology();
    let sub = graph
        .subgraph(&SubgraphQuery {
            seeds: vec!["ap:1".into()],
            k: 2,
            edge_types: None,
            max_nodes: 1000,
        })
        .unwrap();
    let mut ids: Vec<&str> = sub.edges.iter().map(|e| e.id.as_str()).collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![
            "edge:ap:1:BROADCASTS:ssid:1:",
            "edge:ap:1:UPLINK:sw:1:",
            "edge:ssid:1:ON_NETWORK:net:1:",
            "edge:sw:1:UPLINK:gw:1:",
        ]
    );
}
