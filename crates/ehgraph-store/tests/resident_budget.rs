//! Enforcement of the resident-graph budget (register C11, noetl/ehgraph#6).
//!
//! A write that would grow the materialized graphs past `resident_graph_bytes` is
//! refused with `MEMORY_BUDGET_EXCEEDED` **before** anything is logged. Writes that
//! do not grow it still go through. Open never refuses: the log is the authority.

use std::path::Path;

use ehgraph_core::{ErrorCode, NodeInput, WriteRequest};
use ehgraph_store::{DeactivateRequest, Store, StoreConfig};

fn cfg(dir: &Path, cap: u64) -> StoreConfig {
    let mut cfg = StoreConfig::for_tests(dir.to_path_buf());
    cfg.resident_graph_bytes = cap;
    cfg
}

fn upsert(namespace: &str, ids: std::ops::Range<usize>, ts: i64) -> WriteRequest {
    WriteRequest {
        namespace: namespace.into(),
        nodes: ids
            .map(|i| NodeInput {
                id: format!("device:{i:05}"),
                type_name: "Device".into(),
                props: Default::default(),
                origin: None,
                first_seen: Some(ts),
                last_seen: Some(ts),
            })
            .collect(),
        edges: vec![],
        idempotency_key: None,
        source: "budget-test".into(),
        execution_id: "exec".into(),
        ts_ms: ts,
    }
}

fn graph_bytes(store: &Store) -> u64 {
    store
        .summaries()
        .unwrap()
        .iter()
        .map(|s| s.graph_bytes)
        .sum()
}

fn log_bytes(dir: &Path) -> u64 {
    fn walk(p: &Path) -> u64 {
        std::fs::read_dir(p)
            .unwrap()
            .map(|e| {
                let path = e.unwrap().path();
                if path.is_dir() {
                    walk(&path)
                } else if path.to_string_lossy().contains("changelog") {
                    std::fs::metadata(&path).unwrap().len()
                } else {
                    0
                }
            })
            .sum()
    }
    walk(dir)
}

#[test]
fn growth_past_the_cap_is_refused_before_it_is_logged() {
    let dir = tempfile::tempdir().unwrap();
    // Measure what 100 nodes cost, then cap just above it.
    let probe_dir = tempfile::tempdir().unwrap();
    let probe = Store::open(cfg(probe_dir.path(), 0)).unwrap();
    probe.upsert(upsert("lab", 0..100, 1_000)).unwrap();
    let hundred = graph_bytes(&probe);
    assert!(hundred > 0);

    let store = Store::open(cfg(dir.path(), hundred + hundred / 2)).unwrap();
    store.upsert(upsert("lab", 0..100, 1_000)).unwrap();
    assert_eq!(
        graph_bytes(&store),
        hundred,
        "accounting matches a fresh build"
    );
    let head = store.head("lab").unwrap();
    let logged = log_bytes(dir.path());

    // 100 more would take it to ~2x the cost of 100: over the cap.
    let err = store.upsert(upsert("lab", 100..200, 2_000)).unwrap_err();
    assert_eq!(err.code, ErrorCode::MemoryBudgetExceeded, "{}", err.message);
    assert!(
        err.message.contains("resident_graph_bytes"),
        "{}",
        err.message
    );
    assert_eq!(store.head("lab").unwrap(), head, "nothing committed");
    assert_eq!(log_bytes(dir.path()), logged, "nothing logged");

    // The cap is across namespaces: a new namespace cannot dodge it.
    let err = store.upsert(upsert("other", 0..100, 2_000)).unwrap_err();
    assert_eq!(err.code, ErrorCode::MemoryBudgetExceeded);

    // A write that fits still goes through.
    store.upsert(upsert("lab", 100..120, 2_000)).unwrap();

    // Writes that do not grow the graph are never refused: re-seeing existing
    // elements (same-size images) and deactivating them.
    store.upsert(upsert("lab", 0..100, 3_000)).unwrap();
    store
        .deactivate(DeactivateRequest {
            namespace: "lab".into(),
            ids: (0..50).map(|i| format!("device:{i:05}")).collect(),
            origin: None,
            source: None,
            not_seen_since_ms: None,
            idempotency_key: None,
            execution_id: "exec".into(),
            ts_ms: 4_000,
        })
        .unwrap();
}

#[test]
fn open_never_refuses_an_instance_already_over_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = Store::open(cfg(dir.path(), 0)).unwrap();
        store.upsert(upsert("lab", 0..500, 1_000)).unwrap();
    }
    // Reopen with a cap far below what is resident: it opens (the log is the
    // authority), reports the real size, and refuses only growth.
    let store = Store::open(cfg(dir.path(), 1024)).unwrap();
    let resident = graph_bytes(&store);
    assert!(resident > 1024);
    assert_eq!(store.graph_at("lab", None).unwrap().nodes.len(), 500);
    let err = store.upsert(upsert("lab", 500..501, 2_000)).unwrap_err();
    assert_eq!(err.code, ErrorCode::MemoryBudgetExceeded);
    store.upsert(upsert("lab", 0..10, 3_000)).unwrap();
}

#[test]
fn reopen_rebuilds_the_same_accounting() {
    let dir = tempfile::tempdir().unwrap();
    let before = {
        let store = Store::open(cfg(dir.path(), 0)).unwrap();
        store.upsert(upsert("lab", 0..300, 1_000)).unwrap();
        store.upsert(upsert("lab", 100..400, 2_000)).unwrap();
        graph_bytes(&store)
    };
    let store = Store::open(cfg(dir.path(), 0)).unwrap();
    assert_eq!(graph_bytes(&store), before);
}
