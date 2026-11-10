//! `Store::transact`: one atomic, conditional commit (upserts + deactivations as a
//! single graph version), checked under the write lock.

use ehgraph_core::{EdgeInput, ErrorCode, Expect, NodeInput, PropValue};
use ehgraph_store::{Store, StoreConfig, TxRequest};

const TS: i64 = 1_700_000_000_000;

fn open(dir: &std::path::Path) -> Store {
    Store::open(StoreConfig::for_tests(dir.to_path_buf())).unwrap()
}

fn node(id: &str, name: &str) -> NodeInput {
    let mut props = std::collections::BTreeMap::new();
    props.insert("name".to_string(), PropValue::String(name.into()));
    NodeInput {
        id: id.into(),
        type_name: "Device".into(),
        props,
        origin: None,
        first_seen: None,
        last_seen: None,
    }
}

fn edge(src: &str, dst: &str) -> EdgeInput {
    EdgeInput {
        id: None,
        src: src.into(),
        dst: dst.into(),
        type_name: "UPLINK".into(),
        props: Default::default(),
        origin: None,
        first_seen: None,
        last_seen: None,
    }
}

fn tx(nodes: Vec<NodeInput>, edges: Vec<EdgeInput>, deactivate: Vec<&str>) -> TxRequest {
    TxRequest {
        namespace: "lab".into(),
        nodes,
        edges,
        deactivate: deactivate.into_iter().map(String::from).collect(),
        expect: Expect::default(),
        idempotency_key: None,
        source: "fixture".into(),
        execution_id: "exec".into(),
        ts_ms: TS,
    }
}

fn ok(_: &ehgraph_store::MaterializedGraph) -> Result<(), ehgraph_core::EhError> {
    Ok(())
}

fn version_of(store: &Store, id: &str) -> u64 {
    store
        .with_head("lab", |g| {
            g.nodes
                .get(id)
                .map(|n| n.version_updated)
                .or_else(|| g.edges.get(id).map(|e| e.version_updated))
                .unwrap_or(0)
        })
        .unwrap()
}

fn active(store: &Store, id: &str) -> bool {
    store
        .with_head("lab", |g| {
            g.nodes
                .get(id)
                .map(|n| n.active)
                .or_else(|| g.edges.get(id).map(|e| e.active))
                .unwrap_or(false)
        })
        .unwrap()
}

#[test]
fn upserts_and_deactivations_commit_as_one_version_and_replay() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = open(dir.path());
        store
            .transact(
                tx(vec![node("a", "A"), node("b", "B")], vec![], vec![]),
                &ok,
            )
            .unwrap();
        assert_eq!(store.head("lab").unwrap(), 1);
        // One commit: create c and the edge c->a, and deactivate b.
        let out = store
            .transact(
                tx(vec![node("c", "C")], vec![edge("c", "a")], vec!["b"]),
                &ok,
            )
            .unwrap();
        assert_eq!(out.graph_version, 2);
        assert!(out.changed);
        assert_eq!(out.applied, 3);
        assert_eq!(store.head("lab").unwrap(), 2);
        assert!(active(&store, "c") && !active(&store, "b"));
        assert_eq!(version_of(&store, "b"), 2);
        assert_eq!(version_of(&store, "c"), 2);
    }
    // The mixed frame replays after a restart.
    let store = open(dir.path());
    assert_eq!(store.head("lab").unwrap(), 2);
    assert!(active(&store, "c") && !active(&store, "b") && active(&store, "a"));
    assert!(active(&store, "edge:c:UPLINK:a:"));
}

#[test]
fn a_failed_expectation_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store
        .transact(tx(vec![node("a", "A")], vec![], vec![]), &ok)
        .unwrap();
    let v_a = version_of(&store, "a");

    // Element version mismatch.
    let mut req = tx(vec![node("a", "A2"), node("z", "Z")], vec![], vec![]);
    req.expect.versions.insert("a".into(), v_a + 7);
    let err = store.transact(req, &ok).unwrap_err();
    assert_eq!(err.code, ErrorCode::VersionConflict, "{err}");
    assert!(err.message.contains("expectation failed"), "{err}");
    assert_eq!(store.head("lab").unwrap(), 1);
    assert_eq!(version_of(&store, "z"), 0);

    // 0 = must not exist: fails for an existing id, succeeds for a new one.
    let mut req = tx(vec![node("a", "again")], vec![], vec![]);
    req.expect.versions.insert("a".into(), 0);
    assert_eq!(
        store.transact(req, &ok).unwrap_err().code,
        ErrorCode::VersionConflict
    );
    let mut req = tx(vec![node("n", "new")], vec![], vec![]);
    req.expect.versions.insert("n".into(), 0);
    assert_eq!(store.transact(req, &ok).unwrap().graph_version, 2);

    // Graph-version compare-and-set.
    let mut req = tx(vec![node("a", "A3")], vec![], vec![]);
    req.expect.graph_version = Some(1);
    assert_eq!(
        store.transact(req, &ok).unwrap_err().code,
        ErrorCode::VersionConflict
    );
    let mut req = tx(vec![node("a", "A3")], vec![], vec![]);
    req.expect.graph_version = Some(2);
    req.expect.versions.insert("a".into(), v_a);
    assert_eq!(store.transact(req, &ok).unwrap().graph_version, 3);

    // A deactivation bumps the version, so a stale expectation catches a delete.
    store.transact(tx(vec![], vec![], vec!["n"]), &ok).unwrap();
    let mut req = tx(vec![node("n", "revive")], vec![], vec![]);
    req.expect.versions.insert("n".into(), 2);
    assert_eq!(
        store.transact(req, &ok).unwrap_err().code,
        ErrorCode::VersionConflict
    );
}

#[test]
fn the_validator_runs_under_the_lock_and_can_veto() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store
        .transact(tx(vec![node("a", "A")], vec![], vec![]), &ok)
        .unwrap();
    let veto = |g: &ehgraph_store::MaterializedGraph| {
        assert_eq!(g.version, 1, "the validator sees the head");
        Err(ehgraph_core::EhError::new(ErrorCode::VersionConflict, "no"))
    };
    let err = store
        .transact(tx(vec![node("b", "B")], vec![], vec!["a"]), &veto)
        .unwrap_err();
    assert_eq!(err.message, "no");
    assert_eq!(store.head("lab").unwrap(), 1);
    assert!(active(&store, "a"));
    assert_eq!(version_of(&store, "b"), 0);
}

#[test]
fn argument_errors_and_noops() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    // Deactivating in a namespace that does not exist.
    assert_eq!(
        store
            .transact(tx(vec![], vec![], vec!["x"]), &ok)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    store
        .transact(tx(vec![node("a", "A")], vec![], vec![]), &ok)
        .unwrap();
    // Upsert and deactivate the same element in one commit is ambiguous.
    let err = store
        .transact(tx(vec![node("a", "A")], vec![], vec!["a"]), &ok)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    // Identical content: no new version.
    let out = store
        .transact(tx(vec![node("a", "A")], vec![], vec![]), &ok)
        .unwrap();
    assert!(!out.changed);
    assert_eq!(out.graph_version, 1);
    // Idempotency key replays the first outcome.
    let mut req = tx(vec![node("k", "K")], vec![], vec![]);
    req.idempotency_key = Some("key-1".into());
    let first = store.transact(req.clone(), &ok).unwrap();
    let again = store.transact(req, &ok).unwrap();
    assert_eq!(first.graph_version, again.graph_version);
    assert!(!again.changed);
}
