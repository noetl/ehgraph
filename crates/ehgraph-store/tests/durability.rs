use std::fs;

use ehgraph_core::{EdgeInput, NodeInput, Scope, WriteRequest};
use ehgraph_store::{DeactivateRequest, Store, StoreConfig};

fn request(nodes: Vec<NodeInput>, edges: Vec<EdgeInput>) -> WriteRequest {
    WriteRequest {
        namespace: "lab".into(),
        nodes,
        edges,
        idempotency_key: None,
        source: "fixture".into(),
        execution_id: "exec-1".into(),
        ts_ms: 1_700_000_000_000,
    }
}

fn node(id: &str, type_name: &str) -> NodeInput {
    NodeInput {
        id: id.into(),
        type_name: type_name.into(),
        props: Default::default(),
        origin: Some("discovered".into()),
        first_seen: Some(1_700_000_000_000),
        last_seen: Some(1_700_000_000_000),
    }
}

fn open(dir: &std::path::Path) -> Store {
    Store::open(StoreConfig::for_tests(dir.to_path_buf())).unwrap()
}

#[test]
fn upsert_is_idempotent_and_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let batch = request(
        vec![node("device:sw1", "Device"), node("device:sw2", "Device")],
        vec![EdgeInput {
            id: Some("e1".into()),
            src: "device:sw1".into(),
            dst: "device:sw2".into(),
            type_name: "CONNECTED_TO".into(),
            props: Default::default(),
            origin: None,
            first_seen: None,
            last_seen: None,
        }],
    );
    let first = store.upsert(batch.clone()).unwrap();
    assert_eq!(first.graph_version, 1);
    assert!(first.changed);
    let second = store.upsert(batch).unwrap();
    assert_eq!(second.graph_version, 1);
    assert!(!second.changed);

    let mut other = request(vec![node("site:s1", "Site")], vec![]);
    other.namespace = "other".into();
    let written = store.upsert(other).unwrap();
    assert_eq!(written.graph_version, 1);
    assert_eq!(store.graph_at("lab", None).unwrap().nodes.len(), 2);
    assert_eq!(store.graph_at("other", None).unwrap().nodes.len(), 1);
}

#[test]
fn idempotency_key_replays_original_version() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let mut batch = request(vec![node("device:sw1", "Device")], vec![]);
    batch.idempotency_key = Some("batch-1".into());
    let first = store.upsert(batch.clone()).unwrap();
    let mut next = request(vec![node("device:sw2", "Device")], vec![]);
    next.execution_id = "exec-2".into();
    let second = store.upsert(next).unwrap();
    assert_eq!(second.graph_version, 2);
    let replay = store.upsert(batch).unwrap();
    assert_eq!(replay.graph_version, first.graph_version);
    assert!(!replay.changed);
    assert!(store
        .graph_at("lab", None)
        .unwrap()
        .nodes
        .contains_key("device:sw2"));

    let mut changed = request(vec![node("device:sw1", "Device")], vec![]);
    changed.idempotency_key = Some("batch-1".into());
    changed.nodes[0].type_name = "Router".into();
    let err = store.upsert(changed).unwrap_err();
    assert_eq!(err.code, ehgraph_core::ErrorCode::Conflict);
}

#[test]
fn crash_between_append_and_ack_is_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let batch = request(
        vec![node("device:sw1", "Device"), node("device:sw2", "Device")],
        vec![],
    );
    let unacked = store.testing_append_unacked(batch.clone()).unwrap();
    assert_eq!(unacked.graph_version, 1);
    drop(store);

    let store = open(dir.path());
    let graph = store.graph_at("lab", None).unwrap();
    assert_eq!(graph.version, 1);
    assert_eq!(graph.nodes.len(), 2, "a committed frame is fully present");
    let replay = store.upsert(batch).unwrap();
    assert_eq!(replay.graph_version, 1);
    assert!(!replay.changed);

    store.testing_append_raw("lab", b"torn-tail").unwrap();
    drop(store);
    let store = open(dir.path());
    let graph = store.graph_at("lab", None).unwrap();
    assert_eq!(graph.version, 1);
    assert_eq!(graph.nodes.len(), 2, "a torn tail is discarded");
}

#[test]
fn deleting_derived_state_rebuilds_from_log_and_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let v1 = store
        .upsert(request(vec![node("device:sw1", "Device")], vec![]))
        .unwrap();
    let snap = store.create_snapshot("lab", &Scope::Full).unwrap();
    assert_eq!(snap.graph_version, v1.graph_version);
    store
        .upsert(request(vec![node("device:sw2", "Device")], vec![]))
        .unwrap();
    let before = store.graph_at("lab", None).unwrap();
    let at_snap = store.graph_at("lab", Some(v1.graph_version)).unwrap();
    assert_eq!(at_snap.nodes.len(), 1);
    drop(store);

    let derived = dir.path().join("namespaces/lab/derived");
    fs::remove_dir_all(&derived).unwrap();
    fs::create_dir_all(&derived).unwrap();
    fs::write(derived.join("cache.json"), b"this is not authoritative").unwrap();

    let store = open(dir.path());
    let after = store.graph_at("lab", None).unwrap();
    assert_eq!(after.version, before.version);
    assert_eq!(after.nodes, before.nodes);
    assert_eq!(after.edges, before.edges);
    let rebuilt_snap = store.graph_at("lab", Some(v1.graph_version)).unwrap();
    assert_eq!(rebuilt_snap.nodes, at_snap.nodes);
}

#[test]
fn soft_history_deactivate_and_reactivate() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store
        .upsert(request(
            vec![node("device:sw1", "Device"), node("device:old", "Device")],
            vec![],
        ))
        .unwrap();
    let deactivated = store
        .deactivate(DeactivateRequest {
            namespace: "lab".into(),
            ids: vec!["device:old".into()],
            origin: None,
            source: None,
            not_seen_since_ms: None,
            idempotency_key: None,
            execution_id: "exec-1".into(),
            ts_ms: 1_700_000_000_100,
        })
        .unwrap();
    assert!(deactivated.changed);
    let again = store
        .deactivate(DeactivateRequest {
            namespace: "lab".into(),
            ids: vec!["device:old".into()],
            origin: None,
            source: None,
            not_seen_since_ms: None,
            idempotency_key: None,
            execution_id: "exec-1".into(),
            ts_ms: 1_700_000_000_100,
        })
        .unwrap();
    assert!(!again.changed);
    assert_eq!(again.graph_version, deactivated.graph_version);

    let graph = store.graph_at("lab", None).unwrap();
    let old = &graph.nodes["device:old"];
    assert!(!old.active);
    assert_eq!(old.first_seen, 1_700_000_000_000);

    let mut back = request(vec![node("device:old", "Device")], vec![]);
    back.nodes[0].last_seen = Some(1_700_000_000_500);
    let restored = store.upsert(back).unwrap();
    assert!(restored.changed);
    let graph = store.graph_at("lab", None).unwrap();
    let old = &graph.nodes["device:old"];
    assert!(old.active);
    assert_eq!(old.first_seen, 1_700_000_000_000);
    assert_eq!(old.version_created, 1);
}

#[test]
fn deactivate_by_source_and_reject_dangling_edge() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let mut seen = node("device:old", "Device");
    seen.last_seen = Some(10);
    let mut fresh = node("device:new", "Device");
    fresh.last_seen = Some(50);
    store.upsert(request(vec![seen, fresh], vec![])).unwrap();
    let outcome = store
        .deactivate(DeactivateRequest {
            namespace: "lab".into(),
            ids: vec![],
            origin: Some("discovered".into()),
            source: Some("fixture".into()),
            not_seen_since_ms: Some(20),
            idempotency_key: Some("sweep-1".into()),
            execution_id: "exec-9".into(),
            ts_ms: 80,
        })
        .unwrap();
    assert_eq!(outcome.applied, 1);
    let graph = store.graph_at("lab", None).unwrap();
    assert!(!graph.nodes["device:old"].active);
    assert!(graph.nodes["device:new"].active);

    let dangling = request(
        vec![],
        vec![EdgeInput {
            id: None,
            src: "device:old".into(),
            dst: "device:new".into(),
            type_name: "CONNECTED_TO".into(),
            props: Default::default(),
            origin: None,
            first_seen: None,
            last_seen: None,
        }],
    );
    let err = store.upsert(dangling).unwrap_err();
    assert_eq!(err.code, ehgraph_core::ErrorCode::InvalidArgument);
    assert_eq!(store.head("lab").unwrap(), outcome.graph_version);
}

#[test]
fn age_seal_bounds_the_open_part() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = StoreConfig::for_tests(dir.path().to_path_buf());
    cfg.seal_max_age = std::time::Duration::ZERO;
    let store = Store::open(cfg).unwrap();
    store
        .upsert(request(vec![node("device:sw1", "Device")], vec![]))
        .unwrap();
    assert_eq!(store.sealed_version("lab").unwrap(), 1);
    assert_eq!(store.head("lab").unwrap(), 1);
    let plan = store.prepare_backup().unwrap();
    let ns = &plan.namespaces[0];
    assert_eq!(ns.sealed_version, ns.head_version);
    assert!(ns
        .files
        .iter()
        .all(|(_, rel)| !rel.ends_with("active.part")));
}

#[test]
fn strict_schema_rejects_unknown_types() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("schemas")).unwrap();
    fs::write(
        dir.path().join("schemas/lab.json"),
        r#"{"strict": true, "nodes": {"Device": {"required": ["site"]}}}"#,
    )
    .unwrap();
    let store = open(dir.path());
    let err = store
        .upsert(request(vec![node("device:sw1", "Device")], vec![]))
        .unwrap_err();
    assert_eq!(err.code, ehgraph_core::ErrorCode::InvalidArgument);
    let mut ok = node("device:sw1", "Device");
    ok.props
        .insert("site".into(), ehgraph_core::PropValue::String("s1".into()));
    assert!(store.upsert(request(vec![ok], vec![])).unwrap().changed);
}

#[test]
fn data_dir_rejects_a_different_instance() {
    let dir = tempfile::tempdir().unwrap();
    let _store = open(dir.path());
    let mut cfg = StoreConfig::for_tests(dir.path().to_path_buf());
    drop(_store);
    cfg.instance_id = "ehgraph-other".into();
    match Store::open(cfg) {
        Err(err) => assert_eq!(err.code, ehgraph_core::ErrorCode::Conflict),
        Ok(_) => panic!("a data directory must reject a different instance id"),
    }
}
