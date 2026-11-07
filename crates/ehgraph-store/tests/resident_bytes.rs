//! Calibrates the resident-size estimates (`Node::approx_bytes`, `Edge::approx_bytes`)
//! against the real heap, using a counting global allocator (register C11,
//! noetl/ehgraph#6).
//!
//! The graph is `noetl-cluster`-shaped: services, pods and nodes with a handful of
//! string, integer and label-map properties, and edges between them. The test fails
//! if the estimate is more than 25% off the measured heap, so a change to the model
//! or the maps that moves real memory also has to move the estimate.
//!
//! This counts requested bytes. Allocator rounding and fragmentation add more in a
//! real process (typically 10–30%); the budget docs size for that.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};

use ehgraph_core::{Edge, Node, PropValue};
use ehgraph_store::MaterializedGraph;

struct Counting;

static LIVE: AtomicI64 = AtomicI64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as i64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as i64, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(new_size as i64 - layout.size() as i64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The counter is process-wide, so the measuring tests must not overlap.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn props(i: usize, kind: &str) -> BTreeMap<String, PropValue> {
    let mut labels = BTreeMap::new();
    labels.insert(
        "app".into(),
        PropValue::String(format!("{kind}-{}", i % 50)),
    );
    labels.insert("tier".into(), PropValue::String("control-plane".into()));
    let mut p = BTreeMap::new();
    p.insert("name".into(), PropValue::String(format!("{kind}-{i:06}")));
    p.insert("namespace".into(), PropValue::String("noetl".into()));
    p.insert(
        "image".into(),
        PropValue::String(format!(
            "europe-docker.pkg.dev/noetl/server-rust:v3.{}",
            i % 200
        )),
    );
    p.insert("replicas".into(), PropValue::Int((i % 5) as i64));
    p.insert("ready".into(), PropValue::Bool(i % 7 != 0));
    p.insert("labels".into(), PropValue::Map(labels));
    p
}

fn node(i: usize) -> Node {
    let kind = ["Service", "Pod", "Node"][i % 3];
    Node {
        id: format!("{}:{i:06}", kind.to_lowercase()),
        type_name: kind.into(),
        props: props(i, kind),
        first_seen: 1_700_000_000_000,
        last_seen: 1_700_000_000_000 + i as i64,
        active: true,
        origin: Some("k8s-discovery".into()),
        source: "noetl-cluster-scan".into(),
        execution_id: format!("exec-{}", i % 100),
        version_created: 1,
        version_updated: 1,
    }
}

fn edge(i: usize, nodes: &[String]) -> Edge {
    let src = &nodes[i % nodes.len()];
    let dst = &nodes[(i * 7 + 1) % nodes.len()];
    let type_name = ["DISPATCHES_TO", "RUNS_ON", "SELECTS"][i % 3];
    Edge {
        id: format!("{src}->{type_name}->{dst}#{i}"),
        src: src.clone(),
        dst: dst.clone(),
        type_name: type_name.into(),
        props: {
            let mut p = BTreeMap::new();
            p.insert("port".into(), PropValue::Int(8082));
            p
        },
        first_seen: 1_700_000_000_000,
        last_seen: 1_700_000_000_000,
        active: true,
        origin: None,
        source: "noetl-cluster-scan".into(),
        execution_id: format!("exec-{}", i % 100),
        version_created: 1,
        version_updated: 1,
    }
}

#[test]
fn estimates_track_the_measured_heap_for_a_cluster_shaped_graph() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    const NODES: usize = 20_000;
    const EDGES: usize = 40_000;
    let ids: Vec<String> = (0..NODES).map(|i| node(i).id).collect();

    let before = LIVE.load(Ordering::SeqCst);
    let nodes: Vec<Node> = (0..NODES).map(node).collect();
    let edges: Vec<Edge> = (0..EDGES).map(|i| edge(i, &ids)).collect();
    let estimate: u64 = nodes.iter().map(Node::approx_bytes).sum::<u64>()
        + edges.iter().map(Edge::approx_bytes).sum::<u64>();
    let graph = MaterializedGraph::from_elements(1, nodes, edges);
    let measured = (LIVE.load(Ordering::SeqCst) - before) as f64;
    assert_eq!(graph.nodes.len(), NODES);

    let ratio = estimate as f64 / measured;
    let per_element = measured / (NODES + EDGES) as f64;
    println!(
        "resident: measured {:.1} MiB, estimate {:.1} MiB, ratio {ratio:.3}, {per_element:.0} B/element",
        measured / 1048576.0,
        estimate as f64 / 1048576.0
    );
    assert!(
        (0.75..=1.25).contains(&ratio),
        "estimate {estimate} vs measured {measured}: ratio {ratio:.3} is outside 0.75..1.25; \
         recalibrate approx_bytes in ehgraph-core/src/model.rs"
    );
    drop(graph);
}

#[test]
fn estimates_track_the_measured_heap_for_bare_elements() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    const NODES: usize = 20_000;
    const EDGES: usize = 20_000;
    let ids: Vec<String> = (0..NODES).map(|i| format!("n:{i}")).collect();
    let before = LIVE.load(Ordering::SeqCst);
    let nodes: Vec<Node> = (0..NODES)
        .map(|i| Node {
            props: BTreeMap::new(),
            origin: None,
            execution_id: "e".into(),
            source: "s".into(),
            id: format!("n:{i}"),
            ..node(i)
        })
        .collect();
    let edges: Vec<Edge> = (0..EDGES)
        .map(|i| Edge {
            props: BTreeMap::new(),
            execution_id: "e".into(),
            source: "s".into(),
            ..edge(i, &ids)
        })
        .collect();
    let estimate: u64 = nodes.iter().map(Node::approx_bytes).sum::<u64>()
        + edges.iter().map(Edge::approx_bytes).sum::<u64>();
    let graph = MaterializedGraph::from_elements(1, nodes, edges);
    let measured = (LIVE.load(Ordering::SeqCst) - before) as f64;
    let ratio = estimate as f64 / measured;
    println!(
        "bare: measured {:.1} MiB, estimate {:.1} MiB, ratio {ratio:.3}",
        measured / 1048576.0,
        estimate as f64 / 1048576.0
    );
    assert!((0.75..=1.25).contains(&ratio), "ratio {ratio:.3}");
    drop(graph);
}
