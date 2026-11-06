//! Embedded mode: the engine in-process inside a host, no network hop.
//!
//! Covers what embedding changes: grants on in-process calls, one instance shared
//! by in-process calls and host-mounted routes, the single-writer lock inside one
//! process and across processes, lifecycle (shutdown releases the lock, drop is
//! safe), maintenance on the host's runtime, crash durability, and the
//! `auth.mode = "disabled"` guard.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ehgraph::{EhGraph, ErrorCode, GraphConfig, Mode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Per-root tag. Tests run in parallel in one process, and an embedded primary
/// claims its `instance_id` and `domain` process-wide (E2), so every test's
/// instance needs its own identity. Derived from the root so the re-exec'd child
/// opens the same identity as its parent.
fn tag(root: &Path) -> String {
    root.file_name()
        .unwrap()
        .to_string_lossy()
        .trim_start_matches('.')
        .to_string()
}

fn instance_id(root: &Path) -> String {
    format!("ehgraph-embedded-{}", tag(root))
}

fn domain(root: &Path) -> String {
    format!("test-{}", tag(root))
}

fn config_toml(root: &Path, extra: &str) -> String {
    format!(
        r#"
instance_id = "{instance_id}"
domain = "{domain}"
data_dir = "{data}"
backup_prefix = "{backup}"
build = "test"
{extra}

[embedded]
host_replicas = 1

[auth]
mode = "bearer"

[[auth.grants]]
token = "lab-writer"
principal = "fixture"
namespace = "lab"
read = true
write = true

[[auth.grants]]
token = "lab-reader"
principal = "reader"
namespace = "lab"
read = true
write = false

[[auth.grants]]
token = "other-reader"
principal = "other"
namespace = "other"
read = true
write = false

[[auth.grants]]
token = "ops"
principal = "ops"
namespace = "*"
read = true
write = true
"#,
        instance_id = instance_id(root),
        domain = domain(root),
        data = root.join("data").display(),
        backup = root.join("backup").display(),
    )
}

fn config(root: &Path) -> GraphConfig {
    GraphConfig::from_toml(&config_toml(root, "")).unwrap()
}

fn fixture() -> Value {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/small/graph.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn code(envelope: &ehgraph::Envelope) -> Option<ErrorCode> {
    envelope.error.as_ref().map(|err| err.code)
}

#[test]
fn in_process_calls_enforce_per_namespace_grants() {
    let dir = tempfile::tempdir().unwrap();
    let graph = EhGraph::open_embedded(config(dir.path())).unwrap();
    assert_eq!(graph.mode(), Mode::Embedded);

    let writer = graph.client("lab-writer").unwrap();
    let wrote = writer.call("graph.upsert", &fixture());
    assert!(wrote.ok, "{wrote:?}");
    assert_eq!(wrote.graph_version, Some(1));

    // A read-only grant reads, and cannot write.
    let reader = graph.client("lab-reader").unwrap();
    let node = reader.call(
        "graph.node",
        &json!({"namespace": "lab", "id": "device:sw1"}),
    );
    assert!(node.ok, "{node:?}");
    assert_eq!(node.graph_version, Some(1));
    let denied = reader.call("graph.upsert", &fixture());
    assert_eq!(code(&denied), Some(ErrorCode::Unauthorized));

    // A grant on another namespace does not reach this one.
    let other = graph.call(
        "other-reader",
        "graph.node",
        &json!({"namespace": "lab", "id": "device:sw1"}),
    );
    assert_eq!(code(&other), Some(ErrorCode::Unauthorized));

    // An unknown token is refused, both bound and per call.
    assert_eq!(
        graph.client("nope").err().map(|err| err.code),
        Some(ErrorCode::Unauthorized)
    );
    let unknown = graph.call("nope", "graph.instance.info", &json!({}));
    assert_eq!(code(&unknown), Some(ErrorCode::Unauthorized));
}

async fn request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (StatusCode, Value, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = builder
        .body(match body {
            Some(value) => Body::from(serde_json::to_vec(value).unwrap()),
            None => Body::empty(),
        })
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value, text)
}

/// A host (noetl-server) mounts the routes under its own prefix. In-process calls
/// and HTTP calls hit one instance: one log, one `graph_version`, one grant set.
#[tokio::test]
async fn host_mounted_routes_and_in_process_calls_share_one_instance() {
    let dir = tempfile::tempdir().unwrap();
    let graph = EhGraph::open_embedded_async(config(dir.path()))
        .await
        .unwrap();
    let host = axum::Router::new().nest("/ehgraph/test", graph.router());

    let wrote = graph
        .call_async("lab-writer".into(), "graph.upsert".into(), fixture())
        .await;
    assert_eq!(wrote.graph_version, Some(1), "{wrote:?}");

    let (status, node, _) = request(
        &host,
        "POST",
        "/ehgraph/test/v1/tools/graph.node",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab", "id": "device:sw1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{node}");
    assert_eq!(node["graph_version"], 1);

    // The same grants apply on the mounted route.
    let (status, _, _) = request(
        &host,
        "POST",
        "/ehgraph/test/v1/tools/graph.upsert",
        Some("lab-reader"),
        Some(&fixture()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // MCP over the mounted route.
    let (status, rpc, _) = request(
        &host,
        "POST",
        "/ehgraph/test/mcp",
        Some("lab-reader"),
        Some(&json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": "graph.node", "arguments": {"namespace": "lab", "id": "device:sw2"}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rpc["result"]["isError"], false, "{rpc}");

    let (status, health, _) = request(&host, "GET", "/ehgraph/test/healthz", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["mode"], "embedded");
    assert_eq!(health["state"], "serving");

    let (_, _, metrics) = request(&host, "GET", "/ehgraph/test/metrics", None, None).await;
    let (id, dom) = (instance_id(dir.path()), domain(dir.path()));
    assert!(
        metrics.contains(&format!(
            r#"ehgraph_info{{instance_id="{id}",domain="{dom}",mode="embedded"}} 1"#
        )),
        "{metrics}"
    );
    assert!(metrics.contains(&format!(
        r#"ehgraph_graph_version{{instance_id="{id}",domain="{dom}",namespace="lab"}} 1"#
    )));
    graph.shutdown(Duration::from_secs(5)).await;
}

#[test]
fn a_second_handle_in_the_same_process_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let first = EhGraph::open_embedded(config(dir.path())).unwrap();

    let second = EhGraph::open_embedded(config(dir.path()));
    let err = second
        .err()
        .expect("second handle opened on a held data dir");
    assert_eq!(err.code, ErrorCode::Conflict);
    assert!(
        err.message.contains("already open in this process"),
        "{err}"
    );

    // A different spelling of the same directory is the same directory.
    let mut aliased = config(dir.path());
    aliased.data_dir = dir.path().join(".").join("data");
    assert_eq!(
        EhGraph::open_embedded(aliased).err().map(|e| e.code),
        Some(ErrorCode::Conflict)
    );
    // So is a standalone handle in the same process.
    assert_eq!(
        EhGraph::open_standalone(config(dir.path()))
            .err()
            .map(|e| e.code),
        Some(ErrorCode::Conflict)
    );

    drop(first);
    EhGraph::open_embedded(config(dir.path())).expect("lock not released on drop");
}

#[tokio::test]
async fn shutdown_refuses_new_calls_and_releases_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let graph = EhGraph::open_embedded(config(dir.path())).unwrap();
    let host = axum::Router::new().nest("/g", graph.router());
    graph.start_maintenance().unwrap();
    let wrote = graph
        .call_async("lab-writer".into(), "graph.upsert".into(), fixture())
        .await;
    assert!(wrote.ok);

    // A clone the host kept elsewhere (here, the router's state) must not keep the
    // store, or the lock, alive after shutdown.
    let report = graph.shutdown(Duration::from_secs(5)).await;
    assert!(report.lock_released, "{report:?}");
    assert_eq!(report.tasks_stopped, 1, "sweep task not stopped");
    assert!(!graph.is_serving());

    let after = graph.call("lab-reader", "graph.instance.info", &json!({}));
    assert_eq!(code(&after), Some(ErrorCode::Unavailable));
    let (status, health, _) = request(&host, "GET", "/g/healthz", None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(health["state"], "shut_down");
    assert!(graph.start_maintenance().is_err());

    // Idempotent.
    assert!(graph.shutdown(Duration::from_secs(1)).await.lock_released);

    // The directory can be reopened in this process, and the write is durable.
    let reopened = EhGraph::open_embedded(config(dir.path())).unwrap();
    let node = reopened.call(
        "lab-reader",
        "graph.node",
        &json!({"namespace": "lab", "id": "device:sw1"}),
    );
    assert_eq!(node.graph_version, Some(1), "{node:?}");
}

#[test]
fn maintenance_needs_the_hosts_runtime_and_starts_once() {
    let dir = tempfile::tempdir().unwrap();
    let graph = EhGraph::open_embedded(config(dir.path())).unwrap();
    assert_eq!(
        graph.start_maintenance().err().map(|e| e.code),
        Some(ErrorCode::InvalidArgument),
        "maintenance must not invent a runtime inside the host"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        graph.start_maintenance().unwrap();
        assert_eq!(
            graph.start_maintenance().err().map(|e| e.code),
            Some(ErrorCode::Conflict)
        );
    });
    // Dropping the runtime with maintenance running, then the handle, must not hang
    // or leak the lock.
    drop(runtime);
    drop(graph);
    EhGraph::open_embedded(config(dir.path())).unwrap();
}

/// Idle expiry fires from the maintenance loop on the host's runtime, not only
/// when a caller happens to touch the registry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maintenance_expires_idle_sessions_on_the_hosts_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = GraphConfig::from_toml(&format!(
        "{}\n[session]\nidle_timeout_s = 1\nmax_lifetime_s = 60\ngrace_s = 0\n",
        config_toml(dir.path(), "")
    ))
    .unwrap();
    let graph = EhGraph::open_embedded(cfg).unwrap();
    graph.start_maintenance().unwrap();
    let writer = graph.client("lab-writer").unwrap();
    assert!(writer.call_async("graph.upsert", fixture()).await.ok);
    let reader = graph.client("lab-reader").unwrap();
    let opened = reader
        .call_async(
            "graph.session.open",
            json!({"namespace": "lab", "owner": "host", "scope": {"kind": "full"}}),
        )
        .await;
    assert!(opened.ok, "{opened:?}");
    let session = opened.data.unwrap()["graph_session_id"].clone();

    let metrics_before = graph.metrics_text();
    assert!(
        metrics_before.contains(r#"state="active"} 1"#),
        "{metrics_before}"
    );

    // Not touching the session: only the sweep can expire it.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let metrics = graph.metrics_text();
    assert!(
        metrics.contains(r#"state="expired"} 1"#) || metrics.contains(r#"state="active"} 0"#),
        "sweep did not expire the idle session: {metrics}"
    );
    let info = reader
        .call_async("graph.session.info", json!({"graph_session_id": session}))
        .await;
    // `session.info` reports an ended session rather than erroring; what matters is
    // that the sweep ended it and released its memory.
    let data = info.data.clone().unwrap_or_default();
    assert_eq!(data["state"], "expired", "sweep did not expire: {info:?}");
    assert_eq!(data["resident_bytes"], 0, "expired session still resident");
    let read = reader
        .call_async(
            "graph.node",
            json!({"namespace": "lab", "id": "device:sw1", "graph_session_id": session}),
        )
        .await;
    assert_eq!(code(&read), Some(ErrorCode::SessionExpired), "{read:?}");
    graph.shutdown(Duration::from_secs(5)).await;
}

#[test]
fn embedded_refuses_disabled_auth_unless_explicitly_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let disabled = |allow: bool| {
        let mut cfg = config(dir.path());
        cfg.auth.mode = "disabled".into();
        cfg.auth.allow_disabled_embedded = allow;
        cfg
    };
    let err = EhGraph::open_embedded(disabled(false))
        .err()
        .expect("embedded opened with auth disabled");
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(err.message.contains("allow_disabled_embedded"), "{err}");

    // The standalone laptop path is unchanged.
    drop(EhGraph::open_standalone(disabled(false)).unwrap());
    // And an explicit opt-in works for a local-only host.
    let graph = EhGraph::open_embedded(disabled(true)).unwrap();
    assert!(graph.call("", "graph.instance.info", &json!({})).ok);
}

#[test]
fn role_other_than_primary_refuses_to_embed() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = GraphConfig::from_toml(&config_toml(dir.path(), r#"role = "replica""#)).unwrap();
    assert!(EhGraph::open_embedded(cfg).is_err());
}

// ---- cross-process: the test binary re-runs itself as the second process.

const CHILD_ENV: &str = "EHGRAPH_EMBEDDED_CHILD";

/// Child-side entry. A no-op unless the parent set `CHILD_ENV`.
#[test]
fn child_process_entry() {
    let Ok(mode) = std::env::var(CHILD_ENV) else {
        return;
    };
    let root = PathBuf::from(std::env::var("EHGRAPH_EMBEDDED_ROOT").unwrap());
    match mode.as_str() {
        "open" => match EhGraph::open_embedded(config(&root)) {
            Ok(_) => println!("CHILD_OPENED"),
            Err(err) => println!("CHILD_REFUSED {err}"),
        },
        "write-then-crash" => {
            let graph = EhGraph::open_embedded(config(&root)).unwrap();
            let wrote = graph.call("lab-writer", "graph.upsert", &fixture());
            assert!(wrote.ok);
            println!("CHILD_ACKED {}", wrote.graph_version.unwrap());
            std::io::stdout().flush().unwrap();
            // No shutdown, no drop, no unwinding: a host crash.
            std::process::abort();
        }
        "write-tear-crash" => {
            let graph = EhGraph::open_embedded(config(&root)).unwrap();
            let wrote = graph.call("lab-writer", "graph.upsert", &fixture());
            assert!(wrote.ok);
            // The host dies part-way through the NEXT frame: a valid magic and a
            // partial header, no body, no crc.
            let active = root.join("data/namespaces/lab/changelog/active.part");
            assert!(active.exists(), "layout moved: {}", active.display());
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&active)
                .unwrap();
            f.write_all(b"EHGR\x01\x00\x00").unwrap();
            f.sync_all().unwrap();
            println!("CHILD_ACKED {}", wrote.graph_version.unwrap());
            std::io::stdout().flush().unwrap();
            std::process::abort();
        }
        "hold" => {
            let _graph = EhGraph::open_embedded(config(&root)).unwrap();
            println!("CHILD_HOLDING");
            std::io::stdout().flush().unwrap();
            let _ = std::io::stdin().lock().lines().next();
        }
        other => panic!("unknown child mode {other}"),
    }
}

fn child(mode: &str, root: &Path) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "child_process_entry",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(CHILD_ENV, mode)
    .env("EHGRAPH_EMBEDDED_ROOT", root);
    cmd
}

#[test]
fn another_process_cannot_open_a_directory_an_embedding_host_holds() {
    let dir = tempfile::tempdir().unwrap();
    let host = EhGraph::open_embedded(config(dir.path())).unwrap();
    let out = child("open", dir.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("CHILD_REFUSED") && stdout.contains("locked by another ehgraph process"),
        "second process opened a held data dir: {stdout}"
    );
    drop(host);
    let out = child("open", dir.path()).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("CHILD_OPENED"));
}

#[test]
fn an_embedding_host_cannot_open_a_directory_another_process_holds() {
    let dir = tempfile::tempdir().unwrap();
    let mut holder = child("hold", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(holder.stdout.take().unwrap());
    let mut line = String::new();
    while !line.contains("CHILD_HOLDING") {
        line.clear();
        assert!(stdout.read_line(&mut line).unwrap() > 0, "holder exited");
    }
    let err = EhGraph::open_embedded(config(dir.path()))
        .err()
        .expect("host opened a data dir another process holds");
    assert_eq!(err.code, ErrorCode::Conflict);
    // The failed open must not leave a stale in-process claim behind.
    assert!(!ehgraph_store_held(dir.path()));
    holder.stdin.take().unwrap().write_all(b"\n").unwrap();
    holder.wait().unwrap();
    EhGraph::open_embedded(config(dir.path())).unwrap();
}

fn ehgraph_store_held(root: &Path) -> bool {
    ehgraph_store::DirLock::held_in_process(&root.join("data"))
}

/// A host that crashes after an acknowledged write loses nothing: the next open
/// (here, the same directory in a new host process) replays the log.
#[test]
fn an_acknowledged_write_survives_a_host_crash() {
    let dir = tempfile::tempdir().unwrap();
    let out = child("write-then-crash", dir.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "child was meant to abort");
    let acked: u64 = stdout
        .lines()
        .find_map(|l| l.split_once("CHILD_ACKED ").map(|(_, v)| v))
        .unwrap_or_else(|| panic!("child never acked: {stdout}"))
        .trim()
        .parse()
        .unwrap();

    let graph = EhGraph::open_embedded(config(dir.path())).unwrap();
    let node = graph.call(
        "lab-reader",
        "graph.node",
        &json!({"namespace": "lab", "id": "device:sw1"}),
    );
    assert!(node.ok, "{node:?}");
    assert_eq!(node.graph_version, Some(acked));
}

fn second_batch() -> Value {
    let mut batch = fixture();
    batch["execution_id"] = json!("fixture-2");
    batch["nodes"] = json!([{"id": "device:sw9", "type": "Device", "props": {"site": "s1"}, "origin": "discovered"}]);
    batch["edges"] = json!([]);
    batch
}

/// A host crash mid-frame leaves a torn tail. The next host discards it, keeps
/// every acknowledged write, and appends after it — and those appends survive the
/// reopen after that.
#[tokio::test]
async fn a_torn_tail_from_a_host_crash_is_discarded_and_appends_continue() {
    let dir = tempfile::tempdir().unwrap();
    let out = child("write-tear-crash", dir.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("CHILD_ACKED 1"), "{stdout}");

    let graph = EhGraph::open_embedded(config(dir.path())).unwrap();
    let node = graph.call(
        "lab-reader",
        "graph.node",
        &json!({"namespace": "lab", "id": "device:sw1"}),
    );
    assert_eq!(node.graph_version, Some(1), "{node:?}");
    let wrote = graph.call("lab-writer", "graph.upsert", &second_batch());
    assert_eq!(wrote.graph_version, Some(2), "{wrote:?}");
    assert!(graph.shutdown(Duration::from_secs(5)).await.lock_released);

    let graph = EhGraph::open_embedded(config(dir.path())).unwrap();
    for id in ["device:sw1", "device:sw9"] {
        let node = graph.call(
            "lab-reader",
            "graph.node",
            &json!({"namespace": "lab", "id": id}),
        );
        assert!(node.ok, "{id} lost after torn tail + reopen: {node:?}");
        assert_eq!(node.graph_version, Some(2));
    }
}

// ---- E2: an embedded instance is a single writer, guaranteed by declaration.

fn with_embedded(root: &Path, section: &str) -> GraphConfig {
    let text = config_toml(root, "").replace("[embedded]\nhost_replicas = 1\n", section);
    GraphConfig::from_toml(&text).unwrap()
}

fn refusal(cfg: GraphConfig) -> ehgraph::EhError {
    match EhGraph::open_embedded(cfg) {
        Err(err) => err,
        Ok(_) => panic!("an embedded writer opened without a single-writer guarantee"),
    }
}

#[test]
fn embedded_refuses_without_a_declared_single_host() {
    let dir = tempfile::tempdir().unwrap();
    let err = refusal(with_embedded(dir.path(), ""));
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(err.message.contains("host_replicas"), "{err}");
    assert!(err.message.contains("standalone"), "{err}");
    // Nothing was opened: the directory is not locked and was not created.
    assert!(!dir.path().join("data").exists());

    // The standalone server ignores the section entirely.
    drop(EhGraph::open_standalone(with_embedded(dir.path(), "")).unwrap());
}

#[test]
fn embedded_refuses_a_multi_replica_host() {
    let dir = tempfile::tempdir().unwrap();
    let err = refusal(with_embedded(dir.path(), "[embedded]\nhost_replicas = 2\n"));
    assert!(err.message.contains("2 replicas"), "{err}");
    assert!(err.message.contains("standalone"), "{err}");
}

#[test]
fn only_ordinal_zero_and_the_leader_may_write() {
    let dir = tempfile::tempdir().unwrap();
    let err = refusal(with_embedded(
        dir.path(),
        "[embedded]\nhost_replicas = 1\nhost_ordinal = 1\n",
    ));
    assert!(err.message.contains("ordinal 1"), "{err}");
    let err = refusal(with_embedded(
        dir.path(),
        "[embedded]\nhost_replicas = 1\nleader = false\n",
    ));
    assert!(err.message.contains("leader"), "{err}");

    let graph = EhGraph::open_embedded(with_embedded(
        dir.path(),
        "[embedded]\nhost_replicas = 1\nhost_ordinal = 0\nleader = true\n",
    ))
    .unwrap();
    assert!(graph.call("ops", "graph.instance.info", &json!({})).ok);
}

#[test]
fn an_unknown_embedded_key_is_a_config_error_not_a_silent_default() {
    let dir = tempfile::tempdir().unwrap();
    let text = config_toml(dir.path(), "")
        .replace("host_replicas = 1\n", "host_replicas = 1\nreplicas = 3\n");
    assert!(GraphConfig::from_toml(&text).is_err());
}

#[tokio::test]
async fn a_second_embedded_primary_for_the_same_instance_or_domain_fails_fast() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let c = tempfile::tempdir().unwrap();
    let first = EhGraph::open_embedded_async(config(a.path()))
        .await
        .unwrap();

    // Same instance_id, different data directory: the lock cannot see it, the
    // identity claim does.
    let mut same_instance = config(b.path());
    same_instance.instance_id = instance_id(a.path());
    let err = refusal(same_instance);
    assert_eq!(err.code, ErrorCode::Conflict);
    assert!(
        err.message.contains("already open in this process"),
        "{err}"
    );

    // Different instance, same domain: two primaries for one domain.
    let mut same_domain = config(c.path());
    same_domain.domain = domain(a.path());
    let err = refusal(same_domain.clone());
    assert_eq!(err.code, ErrorCode::Conflict);
    assert!(
        err.message
            .contains(&format!("domain {}", domain(a.path()))),
        "{err}"
    );

    // Shutdown releases the claim with the lock; the domain can then open again.
    assert!(first.shutdown(Duration::from_secs(5)).await.lock_released);
    let reopened = EhGraph::open_embedded_async(same_domain).await.unwrap();
    reopened.shutdown(Duration::from_secs(5)).await;
}
