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

/// E9: a restore by an older build refuses a backup a newer build shipped, and
/// refuses BEFORE copying, so the target is not left half-restored.
#[test]
fn restore_refuses_a_backup_from_a_newer_format_without_copying() {
    let dir = tempfile::tempdir().unwrap();
    let backup = tempfile::tempdir().unwrap();
    let store = Store::open(StoreConfig::for_tests(dir.path().to_path_buf())).unwrap();
    write(&store, "lab", "device:sw1");
    let checkpoint = ship(&store, backup.path(), 1_000).unwrap();
    assert_eq!(checkpoint.format, ehgraph_store::format::current_format());
    drop(store);

    let path = backup.path().join("checkpoint.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["format"] = serde_json::Value::from(ehgraph_store::format::STORE_FORMAT + 1);
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

    let target = tempfile::tempdir().unwrap();
    let err = restore(backup.path(), target.path()).unwrap_err();
    assert!(err.message.contains("checkpoint.json"), "{}", err.message);
    assert!(
        err.message.contains("newer than this build"),
        "{}",
        err.message
    );
    assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);
}

/// C02 + C19: a backup prefix never deletes, so after a compaction it still holds the
/// pre-compaction parts. A restore copies them; open recognises them as folded into
/// the floor snapshot and removes them instead of adopting them.
#[test]
fn restore_after_compaction_equals_the_live_graph() {
    let dir = tempfile::tempdir().unwrap();
    let backup = tempfile::tempdir().unwrap();
    let mut cfg = StoreConfig::for_tests(dir.path().to_path_buf());
    cfg.seal_max_age = std::time::Duration::ZERO;
    cfg.idempotency.max_age_ms = 1;
    let store = Store::open(cfg).unwrap();
    for i in 0..12 {
        write(&store, "lab", &format!("device:sw{i}"));
    }
    // First ship: every part goes to the prefix.
    let shipped = ship(&store, backup.path(), 1_000).unwrap();
    let report = store
        .compact("lab", shipped.namespaces["lab"].backed_up_version, 2)
        .unwrap();
    assert!(report.dropped_parts > 0, "{report:?}");
    write(&store, "lab", "device:after");
    ship(&store, backup.path(), 2_000).unwrap();
    let live = store.graph_at("lab", None).unwrap();
    drop(store);

    let target = tempfile::tempdir().unwrap();
    restore(backup.path(), target.path()).unwrap();
    let restored = Store::open(StoreConfig::for_tests(target.path().to_path_buf())).unwrap();
    let got = restored.graph_at("lab", None).unwrap();
    assert_eq!(got.version, live.version);
    assert_eq!(got.nodes, live.nodes);
    let summary = restored
        .summaries()
        .unwrap()
        .into_iter()
        .find(|s| s.namespace == "lab")
        .unwrap();
    assert_eq!(summary.floor_version, report.floor_version);
    let sealed = std::fs::read_dir(target.path().join("namespaces/lab/changelog"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".sealed")
        })
        .count() as u64;
    assert_eq!(
        sealed, summary.log_parts,
        "pre-compaction parts were cleared, not adopted"
    );
}
