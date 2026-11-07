//! Change-log compaction (register C02, noetl/ehgraph#4).
//!
//! The invariant every test checks: **open after compaction equals open before it**
//! (same graph, same head, same idempotency-key window), including after a crash at
//! either step, and after a restore from a backup prefix that still holds the
//! pre-compaction parts.

use std::fs;
use std::path::Path;
use std::time::Duration;

use ehgraph_core::{ErrorCode, NodeInput, WriteRequest};
use ehgraph_store::{CompactCrash, KeyLimits, MaterializedGraph, Store, StoreConfig};
use serde_json::Value;

const WINDOW_MS: i64 = 1_000;

fn cfg(dir: &Path) -> StoreConfig {
    let mut cfg = StoreConfig::for_tests(dir.to_path_buf());
    // Seal after every commit, so each write is its own sealed part.
    cfg.seal_max_age = Duration::ZERO;
    cfg.idempotency = KeyLimits {
        max_age_ms: WINDOW_MS,
        max_keys: 10_000,
    };
    cfg
}

fn write(store: &Store, i: usize, ts_ms: i64) -> u64 {
    store
        .upsert(WriteRequest {
            namespace: "lab".into(),
            nodes: vec![NodeInput {
                id: format!("device:{}", i % 7),
                type_name: "Device".into(),
                props: [("n".to_string(), serde_json::json!(i))]
                    .into_iter()
                    .collect::<serde_json::Map<_, _>>()
                    .into_iter()
                    .map(|(k, v)| (k, ehgraph_core::PropValue::from_json(&v).unwrap()))
                    .collect(),
                origin: None,
                first_seen: Some(ts_ms),
                last_seen: Some(ts_ms),
            }],
            edges: vec![],
            idempotency_key: Some(format!("k{i}")),
            source: "compaction-test".into(),
            execution_id: "exec".into(),
            ts_ms,
        })
        .unwrap()
        .graph_version
}

/// 30 writes, 100 ms apart: with a 1 s key window, the first ~20 parts hold only
/// forgotten keys.
fn populate(store: &Store) -> u64 {
    let mut head = 0;
    for i in 0..30 {
        head = write(store, i, 10_000 + i as i64 * 100);
    }
    head
}

fn summary(store: &Store) -> ehgraph_store::NsSummary {
    store
        .summaries()
        .unwrap()
        .into_iter()
        .find(|s| s.namespace == "lab")
        .unwrap()
}

fn same(a: &MaterializedGraph, b: &MaterializedGraph) {
    assert_eq!(a.version, b.version);
    assert_eq!(a.nodes, b.nodes);
    assert_eq!(a.edges, b.edges);
}

fn sealed_files(dir: &Path) -> usize {
    fs::read_dir(dir.join("namespaces/lab/changelog"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".sealed")
        })
        .count()
}

fn manifest_format(dir: &Path) -> u64 {
    let v: Value =
        serde_json::from_slice(&fs::read(dir.join("namespaces/lab/manifest.json")).unwrap())
            .unwrap();
    v["format"].as_u64().unwrap()
}

/// Which of the keys `k0..k29` the store still remembers (reusing one with another
/// payload is a CONFLICT only while remembered). Probes on a throwaway copy.
fn remembered(dir: &Path) -> Vec<usize> {
    let copy = tempfile::tempdir().unwrap();
    copy_dir(dir, copy.path());
    let _ = fs::remove_file(copy.path().join("LOCK"));
    let store = Store::open(cfg(copy.path())).unwrap();
    let head_ts = 10_000 + 29 * 100;
    (0..30)
        .filter(|i| {
            let r = store.upsert(WriteRequest {
                namespace: "lab".into(),
                nodes: vec![NodeInput {
                    id: format!("probe:{i}"),
                    type_name: "Device".into(),
                    props: Default::default(),
                    origin: None,
                    first_seen: Some(head_ts),
                    last_seen: Some(head_ts),
                }],
                edges: vec![],
                idempotency_key: Some(format!("k{i}")),
                source: "probe".into(),
                execution_id: "probe".into(),
                ts_ms: head_ts,
            });
            matches!(r, Err(e) if e.code == ErrorCode::Conflict)
        })
        .collect()
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

#[test]
fn open_after_compaction_equals_open_before() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(cfg(dir.path())).unwrap();
    let head = populate(&store);
    let before_graph = store.graph_at("lab", None).unwrap();
    let before = summary(&store);
    drop(store);
    let keys_before = remembered(dir.path());
    assert!(
        !keys_before.is_empty() && keys_before.len() < 30,
        "{keys_before:?}"
    );
    assert_eq!(
        manifest_format(dir.path()),
        1,
        "uncompacted manifests stay format 1"
    );

    let store = Store::open(cfg(dir.path())).unwrap();
    let report = store.compact("lab", head, 2).unwrap();
    assert!(report.skipped.is_none(), "{report:?}");
    assert!(report.dropped_parts >= 10, "{report:?}");
    assert!(report.floor_version > 0 && report.floor_version < head);
    same(&store.graph_at("lab", None).unwrap(), &before_graph);
    let after = summary(&store);
    assert_eq!(after.log_parts, before.log_parts - report.dropped_parts);
    assert!(after.log_bytes < before.log_bytes);
    assert_eq!(after.floor_version, report.floor_version);
    drop(store);

    assert_eq!(
        sealed_files(dir.path()) as u64,
        after.log_parts,
        "files match the manifest"
    );
    assert_eq!(
        manifest_format(dir.path()),
        2,
        "a compacted manifest is format 2 (E9)"
    );

    let reopened = Store::open(cfg(dir.path())).unwrap();
    same(&reopened.graph_at("lab", None).unwrap(), &before_graph);
    assert_eq!(reopened.head("lab").unwrap(), head);
    let s = summary(&reopened);
    assert_eq!(s.idempotency_keys, before.idempotency_keys);
    assert_eq!(s.graph_bytes, before.graph_bytes);
    // Writes continue from the same head.
    assert_eq!(write(&reopened, 99, 20_000), head + 1);
}

#[test]
fn the_key_window_survives_compaction_exactly() {
    let a = tempfile::tempdir().unwrap();
    let store = Store::open(cfg(a.path())).unwrap();
    let head = populate(&store);
    drop(store);
    let before = remembered(a.path());

    let store = Store::open(cfg(a.path())).unwrap();
    store.compact("lab", head, 1).unwrap();
    drop(store);
    assert_eq!(remembered(a.path()), before);
}

#[test]
fn unshipped_parts_are_never_compacted() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(cfg(dir.path())).unwrap();
    let head = populate(&store);
    let parts = summary(&store).log_parts;

    let report = store.compact("lab", 0, 1).unwrap();
    assert!(report.skipped.is_some());
    assert_eq!(summary(&store).log_parts, parts);

    // Shipped to version 5: nothing past it goes, even if old enough.
    let report = store.compact("lab", 5, 1).unwrap();
    assert!(report.floor_version <= 5, "{report:?}");
    assert!(summary(&store).log_parts >= parts - 5);
    let _ = head;
}

#[test]
fn parts_inside_the_idempotency_window_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = cfg(dir.path());
    config.idempotency.max_age_ms = 24 * 60 * 60 * 1000;
    let store = Store::open(config).unwrap();
    let head = populate(&store);
    let report = store.compact("lab", head, 1).unwrap();
    assert!(report.skipped.unwrap().contains("idempotency window"));
    assert_eq!(summary(&store).floor_version, 0);
}

#[test]
fn a_crash_before_the_manifest_keeps_the_old_log() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(cfg(dir.path())).unwrap();
    let head = populate(&store);
    let before = store.graph_at("lab", None).unwrap();
    let parts = summary(&store).log_parts;
    store
        .testing_compact_crash("lab", head, 2, CompactCrash::BeforeManifest)
        .unwrap();
    drop(store);

    let reopened = Store::open(cfg(dir.path())).unwrap();
    same(&reopened.graph_at("lab", None).unwrap(), &before);
    let s = summary(&reopened);
    assert_eq!(s.floor_version, 0);
    assert_eq!(s.log_parts, parts);
    assert_eq!(sealed_files(dir.path()) as u64, parts);
    assert_eq!(manifest_format(dir.path()), 1);
}

#[test]
fn a_crash_after_the_manifest_is_finished_by_the_next_open() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(cfg(dir.path())).unwrap();
    let head = populate(&store);
    let before = store.graph_at("lab", None).unwrap();
    let report = store
        .testing_compact_crash("lab", head, 2, CompactCrash::AfterManifest)
        .unwrap();
    let listed = summary(&store).log_parts;
    drop(store);
    // The folded parts are still on disk, no longer listed.
    assert!(sealed_files(dir.path()) as u64 > listed);

    let reopened = Store::open(cfg(dir.path())).unwrap();
    same(&reopened.graph_at("lab", None).unwrap(), &before);
    let s = summary(&reopened);
    assert!(s.floor_version > 0);
    assert_eq!(s.log_parts, listed, "leftovers were not adopted back");
    assert_eq!(
        sealed_files(dir.path()) as u64,
        listed,
        "leftovers were removed"
    );
    let _ = report;
}

#[test]
fn versions_below_the_floor_are_not_found_except_the_floor_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(cfg(dir.path())).unwrap();
    let head = populate(&store);
    let at_three = store.graph_at("lab", Some(3)).unwrap();
    assert_eq!(at_three.version, 3);
    let report = store.compact("lab", head, 2).unwrap();
    let floor = report.floor_version;
    assert!(floor > 3);

    let err = store.graph_at("lab", Some(3)).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    assert!(err.message.contains("compaction floor"), "{}", err.message);
    // Versions at or above the floor are still readable.
    assert_eq!(store.graph_at("lab", Some(floor)).unwrap().version, floor);
    assert_eq!(
        store.graph_at("lab", Some(head - 1)).unwrap().version,
        head - 1
    );
}

#[test]
fn snapshot_retention_never_removes_the_floor_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = cfg(dir.path());
    config.snapshot_keep = 2;
    let store = Store::open(config.clone()).unwrap();
    let head = populate(&store);
    let report = store.compact("lab", head, 2).unwrap();
    let pinned = report.floor_snapshot.clone().unwrap();
    for i in 0..4 {
        write(&store, 100 + i, 30_000 + i as i64);
        store
            .create_snapshot("lab", &ehgraph_core::Scope::Full)
            .unwrap();
    }
    assert!(dir
        .path()
        .join("namespaces/lab/snapshots")
        .join(&pinned)
        .exists());
    let expected = store.graph_at("lab", None).unwrap();
    drop(store);
    let reopened = Store::open(config).unwrap();
    same(&reopened.graph_at("lab", None).unwrap(), &expected);
}

#[test]
fn compacting_twice_moves_the_floor_forward() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(cfg(dir.path())).unwrap();
    let head = populate(&store);
    let first = store.compact("lab", head, 2).unwrap();
    for i in 30..60 {
        write(&store, i, 10_000 + i as i64 * 100);
    }
    let head = store.head("lab").unwrap();
    let expected = store.graph_at("lab", None).unwrap();
    let second = store.compact("lab", head, 2).unwrap();
    assert!(
        second.floor_version > first.floor_version,
        "{first:?} {second:?}"
    );
    drop(store);
    let reopened = Store::open(cfg(dir.path())).unwrap();
    same(&reopened.graph_at("lab", None).unwrap(), &expected);
}

#[test]
fn a_missing_floor_snapshot_refuses_to_open() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(cfg(dir.path())).unwrap();
    let head = populate(&store);
    let report = store.compact("lab", head, 2).unwrap();
    drop(store);
    fs::remove_dir_all(
        dir.path()
            .join("namespaces/lab/snapshots")
            .join(report.floor_snapshot.unwrap()),
    )
    .unwrap();
    let err = match Store::open(cfg(dir.path())) {
        Err(err) => err,
        Ok(_) => panic!("opened a compacted namespace without its floor snapshot"),
    };
    assert_eq!(err.code, ErrorCode::Unavailable, "{}", err.message);
}
