//! The bounded idempotency-key window (register C23, noetl/ehgraph#5).
//!
//! A key is remembered for `max_age_ms` of **log time** (the highest commit `ts_ms`
//! so far) and at most `max_keys` per namespace. The tests tell a remembered key
//! from a forgotten one by reusing it with a *different* payload: a remembered key
//! is a `CONFLICT`, a forgotten one is accepted as a new write.

use std::path::Path;

use ehgraph_core::{ErrorCode, NodeInput, WriteRequest};
use ehgraph_store::{KeyLimits, Store, StoreConfig};

fn cfg(dir: &Path, limits: KeyLimits) -> StoreConfig {
    let mut cfg = StoreConfig::for_tests(dir.to_path_buf());
    cfg.idempotency = limits;
    cfg
}

fn request(key: Option<&str>, id: &str, ts_ms: i64) -> WriteRequest {
    WriteRequest {
        namespace: "lab".into(),
        nodes: vec![NodeInput {
            id: id.into(),
            type_name: "Device".into(),
            props: Default::default(),
            origin: None,
            first_seen: Some(ts_ms),
            last_seen: Some(ts_ms),
        }],
        edges: vec![],
        idempotency_key: key.map(str::to_string),
        source: "idem-test".into(),
        execution_id: "exec".into(),
        ts_ms,
    }
}

/// Reuse `key` with a payload it was never used with.
fn remembered(store: &Store, key: &str, ts_ms: i64) -> bool {
    match store.upsert(request(
        Some(key),
        &format!("device:other-{key}-{ts_ms}"),
        ts_ms,
    )) {
        Err(err) if err.code == ErrorCode::Conflict => true,
        Err(err) => panic!("{key}: {err:?}"),
        Ok(_) => false,
    }
}

fn keys(store: &Store) -> (u64, u64, u64) {
    let s = store
        .summaries()
        .unwrap()
        .into_iter()
        .find(|s| s.namespace == "lab")
        .unwrap();
    (
        s.idempotency_keys,
        s.idempotency_key_bytes,
        s.idempotency_keys_evicted,
    )
}

#[test]
fn a_retry_inside_the_window_returns_the_original_version() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(cfg(dir.path(), KeyLimits::default())).unwrap();
    let first = store
        .upsert(request(Some("k1"), "device:sw1", 1_000))
        .unwrap();
    store.upsert(request(None, "device:sw2", 2_000)).unwrap();
    let retry = store
        .upsert(request(Some("k1"), "device:sw1", 1_000))
        .unwrap();
    assert_eq!(retry.graph_version, first.graph_version);
    assert!(!retry.changed);
    assert!(remembered(&store, "k1", 3_000));
}

#[test]
fn the_count_cap_forgets_the_oldest_keys_first() {
    let dir = tempfile::tempdir().unwrap();
    let limits = KeyLimits {
        max_age_ms: i64::MAX,
        max_keys: 3,
    };
    let store = Store::open(cfg(dir.path(), limits)).unwrap();
    for i in 1..=5 {
        store
            .upsert(request(
                Some(&format!("k{i}")),
                &format!("device:sw{i}"),
                1_000 + i,
            ))
            .unwrap();
    }
    let (count, bytes, evicted) = keys(&store);
    assert_eq!(count, 3);
    assert_eq!(evicted, 2);
    assert!(bytes > 0);
    // k3..k5 are remembered; checking one does not write, so check in order.
    assert!(remembered(&store, "k5", 2_000));
    assert!(remembered(&store, "k3", 2_000));
    // k1 was forgotten: reusing it with another payload is a new write, which
    // itself is remembered and pushes k3 out.
    assert!(!remembered(&store, "k1", 2_000));
    assert_eq!(keys(&store).0, 3);
    assert!(!remembered(&store, "k3", 2_001));
}

#[test]
fn keys_age_out_on_the_log_clock_not_the_wall_clock() {
    let dir = tempfile::tempdir().unwrap();
    let limits = KeyLimits {
        max_age_ms: 1_000,
        max_keys: 1_000,
    };
    // ts_ms values near 1970: by wall clock every key is decades old, but the log
    // clock is what counts, so they are remembered until the log moves on.
    let store = Store::open(cfg(dir.path(), limits)).unwrap();
    store.upsert(request(Some("a"), "device:a", 1_000)).unwrap();
    store.upsert(request(Some("b"), "device:b", 1_500)).unwrap();
    assert!(remembered(&store, "a", 1_900));
    assert_eq!(keys(&store).0, 2);

    // An unkeyed write at 2_400 moves the clock: a (stamp 1_000) is now more than
    // 1_000 ms old, b (stamp 1_500) is not.
    store.upsert(request(None, "device:tick", 2_400)).unwrap();
    assert_eq!(keys(&store), (1, keys(&store).1, 1));
    assert!(!remembered(&store, "a", 2_400));
    assert!(remembered(&store, "b", 2_400));
}

#[test]
fn a_late_timestamp_does_not_move_the_clock_backwards() {
    let dir = tempfile::tempdir().unwrap();
    let limits = KeyLimits {
        max_age_ms: 1_000,
        max_keys: 1_000,
    };
    let store = Store::open(cfg(dir.path(), limits)).unwrap();
    store.upsert(request(Some("a"), "device:a", 1_000)).unwrap();
    store.upsert(request(None, "device:t1", 5_000)).unwrap();
    // A writer with a slow clock: the key it records is stamped 5_000 (the log
    // clock), not 1_200, so it is not born expired.
    store
        .upsert(request(Some("late"), "device:late", 1_200))
        .unwrap();
    assert!(remembered(&store, "late", 1_300));
    assert!(!remembered(&store, "a", 5_000));
}

#[test]
fn reopening_rebuilds_the_same_window_from_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let limits = KeyLimits {
        max_age_ms: 1_000,
        max_keys: 4,
    };
    let script = |store: &Store| {
        for i in 0..6 {
            store
                .upsert(request(
                    Some(&format!("k{i}")),
                    &format!("device:{i}"),
                    1_000 + i * 300,
                ))
                .unwrap();
        }
        // A no-op write with a key leaves an idempotency note, which is replayed too.
        store
            .upsert(request(Some("noop"), "device:5", 2_500))
            .unwrap();
    };
    let live = Store::open(cfg(dir.path(), limits)).unwrap();
    script(&live);
    let before = keys(&live);
    let probe = ["k0", "k1", "k2", "k3", "k4", "k5", "noop"];
    // Inspect without writing: summaries only.
    drop(live);

    let reopened = Store::open(cfg(dir.path(), limits)).unwrap();
    assert_eq!(keys(&reopened), before, "replay rebuilt a different window");

    // And the same keys answer the same way on a store built by replay as on one
    // built live. Build a second live copy to compare against.
    let twin_dir = tempfile::tempdir().unwrap();
    let twin = Store::open(cfg(twin_dir.path(), limits)).unwrap();
    script(&twin);
    for key in probe {
        let a = remembered(&reopened, key, 2_600);
        let b = remembered(&twin, key, 2_600);
        assert_eq!(a, b, "{key}: replayed {a}, live {b}");
    }
}

#[test]
fn a_reused_key_after_the_window_is_accepted_and_documented_as_a_new_write() {
    let dir = tempfile::tempdir().unwrap();
    let limits = KeyLimits {
        max_age_ms: 100,
        max_keys: 10,
    };
    let store = Store::open(cfg(dir.path(), limits)).unwrap();
    let first = store
        .upsert(request(Some("job-1"), "device:a", 1_000))
        .unwrap();
    store.upsert(request(None, "device:tick", 2_000)).unwrap();
    // The same key, same payload, after the window: planned against the current
    // graph. The node is unchanged, so it is a no-op, not a second version.
    let retry = store
        .upsert(request(Some("job-1"), "device:a", 1_000))
        .unwrap();
    assert!(!retry.changed);
    assert!(retry.graph_version > first.graph_version);
}
