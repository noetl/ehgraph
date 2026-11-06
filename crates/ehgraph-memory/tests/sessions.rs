use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ehgraph_core::{Edge, Node, Scope};
use ehgraph_memory::{
    Direction, Limits, NeighborQuery, NodeQuery, OpenRequest, PathQuery, SessionGraph,
    SessionRegistry, SessionState, SubgraphQuery,
};
use ehgraph_store::MaterializedGraph;

fn sample(version: u64) -> SessionGraph {
    let nodes = vec![
        node("site:s1", "Site", version),
        node("device:sw1", "Device", version),
        node("device:sw2", "Device", version),
    ];
    let edges = vec![
        edge("e1", "site:s1", "HAS_DEVICE", "device:sw1", version),
        edge("e2", "site:s1", "HAS_DEVICE", "device:sw2", version),
        edge("e3", "device:sw1", "CONNECTED_TO", "device:sw2", version),
    ];
    let graph = MaterializedGraph::from_elements(version, nodes, edges);
    SessionGraph::from_materialized(&graph).unwrap()
}

fn node(id: &str, type_name: &str, version: u64) -> Node {
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
        version_created: version,
        version_updated: version,
    }
}

fn edge(id: &str, src: &str, type_name: &str, dst: &str, version: u64) -> Edge {
    Edge {
        id: id.into(),
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
        version_created: version,
        version_updated: version,
    }
}

fn open_req(owner: &str, now: i64) -> OpenRequest {
    OpenRequest {
        owner: owner.into(),
        namespace: "lab".into(),
        scope: Scope::Full,
        requested_version: None,
        idle_timeout: Some(Duration::from_millis(1_000)),
        now_ms: now,
    }
}

#[test]
fn queries_use_the_resident_graph() {
    let graph = sample(3);
    assert_eq!(graph.node_batch.num_rows(), 3);
    assert_eq!(graph.edge_batch.num_rows(), 3);
    assert_eq!(graph.node("device:sw1").unwrap().type_name, "Device");
    let neighbors = graph
        .neighbors(&NeighborQuery {
            node_id: "device:sw1".into(),
            edge_type: None,
            direction: Direction::Out,
            limit: 10,
            cursor: None,
            active_only: true,
        })
        .unwrap();
    assert_eq!(neighbors.items.len(), 1);
    let path = graph
        .path(&PathQuery {
            src: "site:s1".into(),
            dst: "device:sw2".into(),
            edge_types: None,
            max_hops: 4,
        })
        .unwrap();
    assert!(path.found);
    assert_eq!(path.nodes, vec!["site:s1", "device:sw2"]);
    let sub = graph
        .subgraph(&SubgraphQuery {
            seeds: vec!["site:s1".into()],
            k: 1,
            edge_types: Some(vec!["HAS_DEVICE".into()]),
            max_nodes: 10,
        })
        .unwrap();
    assert_eq!(sub.nodes.len(), 3);
    let page = graph.query(&NodeQuery {
        type_name: Some("Device".into()),
        origin: None,
        props: Default::default(),
        active: Default::default(),
        limit: 1,
        cursor: None,
    });
    assert!(page.truncated);
    assert_eq!(page.items.len(), 1);
}

#[test]
fn one_load_per_session_and_pinned_version() {
    let reg = SessionRegistry::new(Limits::default());
    let loads = Arc::new(AtomicU64::new(0));
    let loads2 = loads.clone();
    let outcome = reg
        .open(open_req("alice", 0), move || {
            loads2.fetch_add(1, Ordering::SeqCst);
            Ok(sample(1))
        })
        .unwrap();
    assert!(!outcome.reused);
    let again = reg
        .open(open_req("alice", 10), || {
            panic!("second open must reuse the resident graph");
        })
        .unwrap();
    assert!(again.reused);
    assert_eq!(again.graph_session_id, outcome.graph_session_id);
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    let version = reg
        .read(&outcome.graph_session_id, 20, |graph| graph.version)
        .unwrap();
    assert_eq!(version, 1);
}

#[test]
fn concurrent_first_load_is_single_flight() {
    let reg = Arc::new(SessionRegistry::new(Limits::default()));
    let loads = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let reg = reg.clone();
        let loads = loads.clone();
        handles.push(thread::spawn(move || {
            reg.open(open_req("alice", 0), move || {
                loads.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(50));
                Ok(sample(4))
            })
            .unwrap()
        }));
    }
    let ids: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap().graph_session_id)
        .collect();
    assert!(ids.iter().all(|id| id == &ids[0]));
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    assert_eq!(reg.loads().unwrap(), 1);
}

#[test]
fn sessions_do_not_share_copies() {
    let reg = SessionRegistry::new(Limits::default());
    let a = reg.open(open_req("alice", 0), || Ok(sample(1))).unwrap();
    let b = reg.open(open_req("bob", 0), || Ok(sample(1))).unwrap();
    assert_ne!(a.graph_session_id, b.graph_session_id);
    assert_eq!(reg.loads().unwrap(), 2);
    let bytes = reg.resident_bytes().unwrap();
    assert!(bytes >= a.resident_bytes + b.resident_bytes);
}

#[test]
fn refresh_keeps_in_flight_reads_on_the_old_version() {
    let reg = Arc::new(SessionRegistry::new(Limits::default()));
    let opened = reg.open(open_req("alice", 0), || Ok(sample(1))).unwrap();
    let id = opened.graph_session_id.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let reader = reg.clone();
    let read_id = id.clone();
    let handle = thread::spawn(move || {
        reader.read(&read_id, 10, move |graph| {
            started_tx.send(graph.version).unwrap();
            release_rx.recv().unwrap();
            graph.version
        })
    });
    assert_eq!(started_rx.recv().unwrap(), 1);
    let refreshed = reg.refresh(&id, 20, |_| Ok(sample(2))).unwrap();
    assert_eq!(refreshed.previous_version, 1);
    assert_eq!(refreshed.snapshot_version, 2);
    release_tx.send(()).unwrap();
    assert_eq!(handle.join().unwrap().unwrap(), 1);
    let now = reg.read(&id, 30, |graph| graph.version).unwrap();
    assert_eq!(now, 2);
}

#[test]
fn idle_expiry_and_close_reject_later_reads() {
    let reg = SessionRegistry::new(Limits {
        grace: Duration::from_millis(500),
        default_max_lifetime: Duration::from_secs(60),
        ..Limits::default()
    });
    let opened = reg.open(open_req("alice", 0), || Ok(sample(1))).unwrap();
    let id = opened.graph_session_id.clone();
    reg.read(&id, 400, |_| ()).unwrap();
    reg.sweep(1_500).unwrap();
    let err = reg.read(&id, 1_500, |_| ()).unwrap_err();
    assert_eq!(err.code, ehgraph_core::ErrorCode::SessionExpired);
    reg.sweep(1_500 + 500).unwrap();
    assert_eq!(reg.resident_bytes().unwrap(), 0);
    let info = reg.info(&id, 3_000).unwrap();
    assert_eq!(info.state, SessionState::Expired);
    assert_eq!(info.resident_bytes, 0);

    let opened = reg
        .open(open_req("alice", 5_000), || Ok(sample(1)))
        .unwrap();
    let closed = reg.close(&opened.graph_session_id, 5_100).unwrap();
    assert!(closed.released_bytes > 0);
    let err = reg
        .read(&opened.graph_session_id, 5_200, |_| ())
        .unwrap_err();
    assert_eq!(err.code, ehgraph_core::ErrorCode::SessionExpired);
}

#[test]
fn budget_does_not_evict_active_sessions() {
    let tiny = sample(1);
    let bytes = tiny.resident_bytes;
    let reg = SessionRegistry::new(Limits {
        global_bytes: bytes,
        per_session_bytes: bytes,
        default_idle: Duration::from_secs(60),
        default_max_lifetime: Duration::from_secs(60),
        grace: Duration::from_secs(60),
    });
    let first = reg.open(open_req("alice", 0), || Ok(sample(1))).unwrap();
    let err = reg.open(open_req("bob", 1), || Ok(sample(1))).unwrap_err();
    assert_eq!(err.code, ehgraph_core::ErrorCode::MemoryBudgetExceeded);
    assert_eq!(
        reg.read(&first.graph_session_id, 2, |graph| graph.version)
            .unwrap(),
        1
    );
    reg.close(&first.graph_session_id, 3).unwrap();
    let second = reg.open(open_req("bob", 4), || Ok(sample(1))).unwrap();
    assert!(!second.reused);
    assert_eq!(
        reg.evictions().unwrap().get("budget").copied().unwrap_or(0),
        1
    );
    let err = reg
        .open(open_req("carol", 5), || Ok(sample(1)))
        .unwrap_err();
    assert_eq!(err.code, ehgraph_core::ErrorCode::MemoryBudgetExceeded);
}

#[test]
fn per_session_cap_refuses_without_installing_a_session() {
    let reg = SessionRegistry::new(Limits {
        global_bytes: u64::MAX / 4,
        per_session_bytes: 1,
        default_idle: Duration::from_secs(60),
        default_max_lifetime: Duration::from_secs(60),
        grace: Duration::from_secs(60),
    });
    let err = reg
        .open(open_req("alice", 0), || Ok(sample(1)))
        .unwrap_err();
    assert_eq!(err.code, ehgraph_core::ErrorCode::MemoryBudgetExceeded);
    assert_eq!(reg.resident_bytes().unwrap(), 0);
    assert!(reg.list(None, None, 0).unwrap().is_empty());
}

#[test]
fn scale_fixture_loads_within_the_smoke_budget() {
    let nodes: Vec<Node> = (0..10_000)
        .map(|i| node(&format!("n{i}"), "Node", 1))
        .collect();
    let edges: Vec<Edge> = (0..20_000)
        .map(|i| {
            let src = i % 10_000;
            let dst = (i * 7 + 1) % 10_000;
            edge(
                &format!("e{i}"),
                &format!("n{src}"),
                "LINK",
                &format!("n{dst}"),
                1,
            )
        })
        .collect();
    let materialized = MaterializedGraph::from_elements(1, nodes, edges);
    let started = std::time::Instant::now();
    let graph = SessionGraph::from_materialized(&materialized).unwrap();
    let load = started.elapsed();
    assert!(load < Duration::from_secs(5), "scale load took {load:?}");
    assert!(graph.resident_bytes > 0);
    let started = std::time::Instant::now();
    let page = graph
        .neighbors(&NeighborQuery {
            node_id: "n1".into(),
            edge_type: None,
            direction: Direction::Both,
            limit: 32,
            cursor: None,
            active_only: true,
        })
        .unwrap();
    let query = started.elapsed();
    assert!(!page.items.is_empty());
    assert!(
        query < Duration::from_millis(500),
        "neighbor query took {query:?}"
    );
}
