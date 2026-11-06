use ehgraph_backup::{restore, ship};
use ehgraph_core::{NodeInput, WriteRequest};
use ehgraph_store::{Store, StoreConfig};

fn node(id: &str) -> NodeInput {
    NodeInput {
        id: id.into(),
        type_name: "Device".into(),
        props: Default::default(),
        origin: None,
        first_seen: Some(10),
        last_seen: Some(10),
    }
}

fn write(store: &Store, namespace: &str, id: &str) -> u64 {
    store
        .upsert(WriteRequest {
            namespace: namespace.into(),
            nodes: vec![node(id)],
            edges: vec![],
            idempotency_key: None,
            source: "backup-test".into(),
            execution_id: "exec".into(),
            ts_ms: 10,
        })
        .unwrap()
        .graph_version
}

#[test]
fn restore_reproduces_the_backed_up_version() {
    let dir = tempfile::tempdir().unwrap();
    let backup = tempfile::tempdir().unwrap();
    let store = Store::open(StoreConfig::for_tests(dir.path().to_path_buf())).unwrap();
    let version = write(&store, "lab", "device:sw1");
    write(&store, "lab", "device:sw2");
    let checkpoint = ship(&store, backup.path(), 1_000).unwrap();
    assert_eq!(
        checkpoint.namespaces["lab"].backed_up_version,
        store.head("lab").unwrap()
    );
    let before = store.graph_at("lab", None).unwrap();
    drop(store);

    let restored_dir = tempfile::tempdir().unwrap();
    restore(backup.path(), restored_dir.path()).unwrap();
    let restored = Store::open(StoreConfig::for_tests(restored_dir.path().to_path_buf())).unwrap();
    let after = restored.graph_at("lab", None).unwrap();
    assert_eq!(after.version, before.version);
    assert_eq!(after.nodes, before.nodes);
    assert_eq!(after.version, version.max(before.version));
}

#[test]
fn two_instances_stay_isolated_across_backup_and_restore() {
    let a_dir = tempfile::tempdir().unwrap();
    let b_dir = tempfile::tempdir().unwrap();
    let backup = tempfile::tempdir().unwrap();
    let mut a_cfg = StoreConfig::for_tests(a_dir.path().to_path_buf());
    a_cfg.instance_id = "ehgraph-network".into();
    a_cfg.domain = "network".into();
    let mut b_cfg = StoreConfig::for_tests(b_dir.path().to_path_buf());
    b_cfg.instance_id = "ehgraph-noetl-cluster".into();
    b_cfg.domain = "noetl-cluster".into();
    let a = Store::open(a_cfg).unwrap();
    let b = Store::open(b_cfg.clone()).unwrap();
    write(&a, "qnetmon", "device:sw1");
    write(&b, "cluster", "service:api");
    let b_before = b.graph_at("cluster", None).unwrap();
    ship(&a, backup.path(), 5_000).unwrap();
    drop(a);

    let empty = tempfile::tempdir().unwrap();
    restore(backup.path(), empty.path()).unwrap();
    let mut restored_cfg = StoreConfig::for_tests(empty.path().to_path_buf());
    restored_cfg.instance_id = "ehgraph-network".into();
    restored_cfg.domain = "network".into();
    let restored = Store::open(restored_cfg).unwrap();
    assert!(restored
        .graph_at("qnetmon", None)
        .unwrap()
        .nodes
        .contains_key("device:sw1"));
    assert!(restored.graph_at("cluster", None).is_err());

    let b_after = b.graph_at("cluster", None).unwrap();
    assert_eq!(b_after.nodes, b_before.nodes);
    assert_eq!(b_after.version, b_before.version);
    drop(b);
    // Undeploy of A (dropping its directory) does not affect B.
    drop(a_dir);
    let b_again = Store::open(b_cfg).unwrap();
    assert_eq!(
        b_again.graph_at("cluster", None).unwrap().nodes,
        b_before.nodes
    );
}
