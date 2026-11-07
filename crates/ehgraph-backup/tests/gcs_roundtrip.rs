//! GCS backup target (C19, noetl/ehgraph#8) against an in-process fake of the GCS
//! JSON API. The fake enforces what the real service does and the backup relies
//! on: `ifGenerationMatch=0` answers 412 when the object exists, uploads replace
//! whole objects, and listing is paginated. It also records every request, so the
//! tests can check the ship order (immutable objects, then head, then checkpoint
//! last) and that every request carried the injected credentials.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use ehgraph_backup::{open_target, Backup, ObjectStore, TargetConfig, TokenProvider};
use ehgraph_core::{EhError, NodeInput, WriteRequest};
use ehgraph_store::{Store, StoreConfig};
use serde_json::{json, Value};

#[derive(Default)]
struct Bucket {
    objects: BTreeMap<String, Vec<u8>>,
    /// (method, object name, if_generation_match) in arrival order.
    log: Vec<(String, String, Option<String>)>,
    /// Bearer tokens seen; `None` for a request without one.
    auth: Vec<Option<String>>,
    /// Fail this many upcoming uploads with 503 (a transient error).
    fail_uploads: usize,
}

type Shared = Arc<Mutex<Bucket>>;

struct Fake {
    addr: SocketAddr,
    bucket: Shared,
    _runtime: tokio::runtime::Runtime,
}

impl Fake {
    fn start() -> Self {
        let bucket: Shared = Arc::default();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let app = Router::new()
            .route("/upload/storage/v1/b/{bucket}/o", post(upload))
            .route("/storage/v1/b/{bucket}/o", get(list))
            .route("/storage/v1/b/{bucket}/o/{object}", get(download))
            .with_state(bucket.clone());
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        runtime.spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            addr,
            bucket,
            _runtime: runtime,
        }
    }

    fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn with<T>(&self, f: impl FnOnce(&mut Bucket) -> T) -> T {
        f(&mut self.bucket.lock().unwrap())
    }
}

fn record(bucket: &mut Bucket, headers: &HeaderMap, method: &str, name: &str, pre: Option<String>) {
    bucket.log.push((method.into(), name.into(), pre));
    bucket.auth.push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
    );
}

async fn upload(
    State(state): State<Shared>,
    Path(_bucket): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut b = state.lock().unwrap();
    let name = q.get("name").cloned().unwrap_or_default();
    let pre = q.get("ifGenerationMatch").cloned();
    record(&mut b, &headers, "upload", &name, pre.clone());
    if b.fail_uploads > 0 {
        b.fail_uploads -= 1;
        return (StatusCode::SERVICE_UNAVAILABLE, "try again").into_response();
    }
    assert_eq!(q.get("uploadType").map(String::as_str), Some("media"));
    if pre.as_deref() == Some("0") && b.objects.contains_key(&name) {
        return (StatusCode::PRECONDITION_FAILED, "conditionNotMet").into_response();
    }
    b.objects.insert(name.clone(), body.to_vec());
    axum::Json(json!({"name": name, "generation": "1"})).into_response()
}

async fn download(
    State(state): State<Shared>,
    Path((_bucket, object)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let mut b = state.lock().unwrap();
    record(&mut b, &headers, "get", &object, None);
    match b.objects.get(&object) {
        None => (StatusCode::NOT_FOUND, "No such object").into_response(),
        Some(bytes) if q.get("alt").map(String::as_str) == Some("media") => {
            bytes.clone().into_response()
        }
        Some(_) => axum::Json(json!({"name": object})).into_response(),
    }
}

async fn list(
    State(state): State<Shared>,
    Path(_bucket): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let mut b = state.lock().unwrap();
    record(&mut b, &headers, "list", "", None);
    let prefix = q.get("prefix").cloned().unwrap_or_default();
    let names: Vec<_> = b
        .objects
        .keys()
        .filter(|k| k.starts_with(&prefix))
        .cloned()
        .collect();
    // Two per page, so restore has to follow nextPageToken.
    let start: usize = q.get("pageToken").map(|t| t.parse().unwrap()).unwrap_or(0);
    let page: Vec<_> = names
        .iter()
        .skip(start)
        .take(2)
        .map(|n| json!({"name": n}))
        .collect();
    let mut reply = json!({"items": page});
    if start + 2 < names.len() {
        reply["nextPageToken"] = Value::from((start + 2).to_string());
    }
    axum::Json(reply).into_response()
}

#[derive(Debug)]
struct HostToken;

impl TokenProvider for HostToken {
    fn bearer(&self) -> Result<Option<String>, EhError> {
        Ok(Some("host-token".into()))
    }
}

fn target(fake: &Fake, url: &str) -> Arc<dyn ObjectStore> {
    let config = TargetConfig {
        endpoint: fake.endpoint(),
        auth: "none".into(),
        timeout_s: 5,
    };
    // The host's provider overrides `auth = "none"`: this is the embedded path.
    open_target(url, &config, Some(Arc::new(HostToken))).unwrap()
}

fn write(store: &Store, namespace: &str, id: &str) {
    store
        .upsert(WriteRequest {
            namespace: namespace.into(),
            nodes: vec![NodeInput {
                id: id.into(),
                type_name: "Device".into(),
                props: Default::default(),
                origin: None,
                first_seen: Some(10),
                last_seen: Some(10),
            }],
            edges: vec![],
            idempotency_key: None,
            source: "gcs-test".into(),
            execution_id: "exec".into(),
            ts_ms: 10,
        })
        .unwrap();
}

fn store(dir: &std::path::Path) -> Store {
    Store::open(StoreConfig::for_tests(dir.to_path_buf())).unwrap()
}

#[test]
fn ship_and_restore_round_trip_through_gcs() {
    let fake = Fake::start();
    let dir = tempfile::tempdir().unwrap();
    let live = store(dir.path());
    for i in 0..3 {
        write(&live, "lab", &format!("device:sw{i}"));
        write(&live, "core", &format!("device:r{i}"));
    }
    let backup = Backup::new(target(&fake, "gs://noetl-ehgraph/ehgraph/ehgraph-network"));
    let checkpoint = backup.ship(&live, 1_000).unwrap();
    assert_eq!(checkpoint.namespaces.len(), 2);
    assert_eq!(backup.cached_checkpoint(), Some(checkpoint.clone()));
    let before_lab = live.graph_at("lab", None).unwrap();
    let before_core = live.graph_at("core", None).unwrap();
    drop(live);

    // Every object lives under the prefix, and every request carried the host token.
    fake.with(|b| {
        assert!(b
            .objects
            .keys()
            .all(|k| k.starts_with("ehgraph/ehgraph-network/")));
        assert!(b
            .objects
            .contains_key("ehgraph/ehgraph-network/checkpoint.json"));
        assert!(b
            .auth
            .iter()
            .all(|a| a.as_deref() == Some("Bearer host-token")));
    });

    // Order: checkpoint is the last write; each head follows that namespace's parts.
    fake.with(|b| {
        let uploads: Vec<_> = b.log.iter().filter(|(m, ..)| m == "upload").collect();
        assert!(uploads.last().unwrap().1.ends_with("/checkpoint.json"));
        for ns in ["lab", "core"] {
            let head = uploads
                .iter()
                .position(|(_, n, _)| n.ends_with(&format!("namespaces/{ns}/head.json")))
                .unwrap();
            let manifest = uploads
                .iter()
                .position(|(_, n, _)| n.ends_with(&format!("namespaces/{ns}/manifest.json")))
                .unwrap();
            let last_immutable = uploads
                .iter()
                .rposition(|(_, n, pre)| {
                    n.contains(&format!("namespaces/{ns}/")) && pre.as_deref() == Some("0")
                })
                .unwrap();
            assert!(
                last_immutable < manifest && manifest < head,
                "{ns}: {uploads:?}"
            );
        }
        // Mutable objects are plain uploads; immutable ones are preconditioned.
        for (_, name, pre) in &uploads {
            let mutable = name.ends_with("head.json")
                || name.ends_with("manifest.json")
                || name.ends_with("checkpoint.json");
            assert_eq!(pre.is_none(), mutable, "{name}");
        }
    });

    let restored = tempfile::tempdir().unwrap();
    let fresh = Backup::new(target(&fake, "gs://noetl-ehgraph/ehgraph/ehgraph-network/"));
    assert_eq!(fresh.checkpoint().unwrap(), Some(checkpoint.clone()));
    let got = fresh.restore(restored.path()).unwrap();
    assert_eq!(got, checkpoint);
    assert!(!restored.path().join("checkpoint.json").exists());
    let reopened = store(restored.path());
    let after_lab = reopened.graph_at("lab", None).unwrap();
    let after_core = reopened.graph_at("core", None).unwrap();
    assert_eq!(after_lab.version, before_lab.version);
    assert_eq!(after_lab.nodes, before_lab.nodes);
    assert_eq!(after_core.nodes, before_core.nodes);
}

#[test]
fn a_second_ship_never_rewrites_an_immutable_object() {
    let fake = Fake::start();
    let dir = tempfile::tempdir().unwrap();
    let live = store(dir.path());
    write(&live, "lab", "device:sw1");
    let backup = Backup::new(target(&fake, "gs://b/p"));
    backup.ship(&live, 1_000).unwrap();
    let immutable: Vec<String> = fake.with(|b| {
        b.log
            .iter()
            .filter(|(m, _, pre)| m == "upload" && pre.as_deref() == Some("0"))
            .map(|(_, n, _)| n.clone())
            .collect()
    });
    assert!(!immutable.is_empty());
    // Mark every shipped immutable object; a rewrite would clobber the mark.
    fake.with(|b| {
        for name in &immutable {
            b.objects.insert(name.clone(), b"marker".to_vec());
        }
        b.log.clear();
    });
    write(&live, "lab", "device:sw2");
    let second = backup.ship(&live, 2_000).unwrap();
    assert_eq!(
        second.namespaces["lab"].backed_up_version,
        live.head("lab").unwrap()
    );
    fake.with(|b| {
        for name in &immutable {
            assert_eq!(b.objects[name], b"marker", "{name} was rewritten");
        }
    });

    // The precondition itself: a put-if-absent that races past the existence check
    // gets 412 and reports "already there" without replacing anything.
    let raw = target(&fake, "gs://b/p");
    let key = immutable[0].strip_prefix("p/").unwrap();
    assert!(!raw.put_if_absent(key, b"clobber").unwrap());
    fake.with(|b| assert_eq!(b.objects[&immutable[0]], b"marker"));
    assert!(raw.put_if_absent("fresh/object", b"x").unwrap());
}

#[test]
fn transient_errors_are_retried() {
    let fake = Fake::start();
    let dir = tempfile::tempdir().unwrap();
    let live = store(dir.path());
    write(&live, "lab", "device:sw1");
    fake.with(|b| b.fail_uploads = 2);
    let checkpoint = Backup::new(target(&fake, "gs://b/p"))
        .ship(&live, 1_000)
        .unwrap();
    assert_eq!(checkpoint.namespaces["lab"].backed_up_version, 1);
    fake.with(|b| assert!(b.objects.contains_key("p/checkpoint.json")));
}

#[test]
fn restore_reads_only_its_own_prefix() {
    let fake = Fake::start();
    let dir = tempfile::tempdir().unwrap();
    let live = store(dir.path());
    write(&live, "lab", "device:sw1");
    Backup::new(target(&fake, "gs://b/net"))
        .ship(&live, 1_000)
        .unwrap();
    // A sibling prefix whose name starts the same way.
    fake.with(|b| {
        b.objects.insert(
            "network-other/namespaces/x/head.json".into(),
            b"{}".to_vec(),
        );
        b.objects
            .insert("net-2/checkpoint.json".into(), b"{}".to_vec());
    });
    let restored = tempfile::tempdir().unwrap();
    Backup::new(target(&fake, "gs://b/net"))
        .restore(restored.path())
        .unwrap();
    assert!(!restored.path().join("namespaces/x").exists());
    assert!(store(restored.path()).graph_at("lab", None).is_ok());
}

/// E9 through GCS: a newer-format checkpoint refuses before anything is written.
#[test]
fn restore_refuses_a_newer_format_checkpoint_without_writing() {
    let fake = Fake::start();
    let dir = tempfile::tempdir().unwrap();
    let live = store(dir.path());
    write(&live, "lab", "device:sw1");
    Backup::new(target(&fake, "gs://b/p"))
        .ship(&live, 1_000)
        .unwrap();
    fake.with(|b| {
        let key = "p/checkpoint.json".to_string();
        let mut value: Value = serde_json::from_slice(&b.objects[&key]).unwrap();
        value["format"] = Value::from(ehgraph_store::format::STORE_FORMAT + 1);
        b.objects.insert(key, serde_json::to_vec(&value).unwrap());
    });
    let restored = tempfile::tempdir().unwrap();
    let err = Backup::new(target(&fake, "gs://b/p"))
        .restore(restored.path())
        .unwrap_err();
    assert!(
        err.message.contains("gs://b/p/checkpoint.json"),
        "{}",
        err.message
    );
    assert!(
        err.message.contains("newer than this build"),
        "{}",
        err.message
    );
    assert_eq!(std::fs::read_dir(restored.path()).unwrap().count(), 0);
}

#[test]
fn an_empty_bucket_has_no_checkpoint_and_restore_says_so() {
    let fake = Fake::start();
    let backup = Backup::new(target(&fake, "gs://b/empty"));
    assert_eq!(backup.checkpoint().unwrap(), None);
    let restored = tempfile::tempdir().unwrap();
    let err = backup.restore(restored.path()).unwrap_err();
    assert!(
        err.message.contains("no checkpoint in gs://b/empty"),
        "{}",
        err.message
    );
}

#[test]
fn unsupported_schemes_and_bad_auth_are_config_errors() {
    let config = TargetConfig::default();
    assert!(open_target("s3://bucket/p", &config, None).is_err());
    let bad = TargetConfig {
        auth: "password".into(),
        ..TargetConfig::default()
    };
    assert!(open_target("gs://b/p", &bad, None).is_err());
    // A directory stays a directory.
    let dir = tempfile::tempdir().unwrap();
    let fs = open_target(dir.path().to_str().unwrap(), &config, None).unwrap();
    assert_eq!(fs.describe(), dir.path().display().to_string());
}
