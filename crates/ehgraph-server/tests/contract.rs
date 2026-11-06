use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ehgraph_server::{boot, Config};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

struct Harness {
    app: axum::Router,
    _dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("ehgraph.toml");
    let data = dir.path().join("data");
    let backup = dir.path().join("backup");
    std::fs::write(
        &config_path,
        format!(
            r#"
instance_id = "ehgraph-test"
domain = "test"
data_dir = "{data}"
backup_prefix = "{backup}"
listen = "127.0.0.1:9"
role = "primary"
build = "test"
backup_interval_s = 0

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
token = "ops"
principal = "ops"
namespace = "*"
read = true
write = true
"#,
            data = data.display(),
            backup = backup.display(),
        ),
    )
    .unwrap();
    let running = boot(Config::load(&config_path).unwrap()).unwrap();
    Harness {
        app: running.router,
        _dir: dir,
    }
}

async fn request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if token.is_some() || body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
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
    let value: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, value)
}

fn fixture() -> Value {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/small/graph.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[tokio::test]
async fn every_contract_tool_roundtrips() {
    let harness = harness().await;
    let app = &harness.app;
    let (status, health) = request(app, "GET", "/healthz", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["instance_id"], "ehgraph-test");
    assert_eq!(health["domain"], "test");
    assert_eq!(health["role"], "primary");
    assert_eq!(health["state"], "serving");
    assert_eq!(health["mode"], "standalone");

    let (status, listed) = request(app, "GET", "/mcp/tools", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for required in [
        "graph.upsert",
        "graph.deactivate",
        "graph.snapshot.create",
        "graph.node",
        "graph.edge",
        "graph.query",
        "graph.neighbors",
        "graph.subgraph",
        "graph.path",
        "graph.snapshot.info",
        "graph.session.open",
        "graph.session.refresh",
        "graph.session.close",
        "graph.session.info",
        "graph.session.list",
        "graph.instance.info",
        "graph.backup.status",
    ] {
        assert!(names.contains(&required), "missing tool {required}");
    }

    let (status, denied) = request(
        app,
        "POST",
        "/v1/tools/graph.instance.info",
        None,
        Some(&json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(denied["error"]["code"], "UNAUTHORIZED");

    let batch = fixture();
    let (status, upserted) = request(
        app,
        "POST",
        "/v1/tools/graph.upsert",
        Some("lab-writer"),
        Some(&batch),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{upserted}");
    assert_eq!(upserted["graph_version"], 1);
    assert_eq!(upserted["ok"], true);
    assert_eq!(upserted["instance_id"], "ehgraph-test");
    assert_eq!(upserted["domain"], "test");

    let (status, replay) = request(
        app,
        "POST",
        "/v1/tools/graph.upsert",
        Some("lab-writer"),
        Some(&batch),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["graph_version"], 1);
    assert_eq!(replay["data"]["changed"], false);

    let (status, node) = request(
        app,
        "POST",
        "/v1/tools/graph.node",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab", "id": "device:sw1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(node["data"]["type"], "Device");
    assert_eq!(node["graph_version"], 1);

    let (status, edge) = request(
        app,
        "POST",
        "/v1/tools/graph.edge",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab", "id": "e3"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edge["data"]["type"], "CONNECTED_TO");

    let (status, query) = request(
        app,
        "POST",
        "/v1/tools/graph.query",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab", "type": "Interface", "props": {"mtu": {"gte": 1500}}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(query["data"]["nodes"].as_array().unwrap().len(), 2);

    let (status, neighbors) = request(
        app,
        "POST",
        "/v1/tools/graph.neighbors",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab", "node_id": "device:sw1", "direction": "out"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let neighbor_ids: Vec<&str> = neighbors["data"]["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| edge["id"].as_str().unwrap())
        .collect();
    assert_eq!(neighbor_ids, vec!["e1", "e3"]);

    let (status, path) = request(
        app,
        "POST",
        "/v1/tools/graph.path",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab", "src": "site:s1", "dst": "iface:sw1-eth0"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{path}");
    assert_eq!(
        path["data"]["nodes"],
        json!(["site:s1", "device:sw1", "iface:sw1-eth0"])
    );

    let (status, sub) = request(
        app,
        "POST",
        "/v1/tools/graph.subgraph",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab", "seeds": ["site:s1"], "k": 1, "edge_types": ["HAS_DEVICE"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sub["data"]["nodes"].as_array().unwrap().len(), 3);

    let (status, snap) = request(
        app,
        "POST",
        "/v1/tools/graph.snapshot.create",
        Some("lab-writer"),
        Some(&json!({"namespace": "lab", "scope": {"kind": "full"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{snap}");
    let (status, info) = request(
        app,
        "POST",
        "/v1/tools/graph.snapshot.info",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["data"]["nodes"], 5);
    assert_eq!(info["data"]["edges"], 5);

    let (status, opened) = request(
        app,
        "POST",
        "/v1/tools/graph.session.open",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab", "owner": "alice", "scope": {"kind": "full"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{opened}");
    let session = opened["data"]["graph_session_id"].as_str().unwrap();
    assert_eq!(opened["data"]["snapshot_version"], 1);
    assert_eq!(opened["data"]["reused"], false);

    let (status, again) = request(
        app,
        "POST",
        "/v1/tools/graph.session.open",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab", "owner": "alice", "scope": {"kind": "full"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["data"]["reused"], true);

    let second = json!({
        "namespace": "lab",
        "source": "fixture",
        "execution_id": "fixture-2",
        "ts_ms": 1_700_000_001_000_i64,
        "nodes": [{"id": "device:sw3", "type": "Device"}],
        "edges": []
    });
    let (status, wrote) = request(
        app,
        "POST",
        "/v1/tools/graph.upsert",
        Some("lab-writer"),
        Some(&second),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{wrote}");
    assert_eq!(wrote["graph_version"], 2);

    let (status, pinned) = request(
        app,
        "POST",
        "/v1/tools/graph.node",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab", "id": "device:sw1", "graph_session_id": session})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{pinned}");
    assert_eq!(pinned["graph_version"], 1);
    let (status, missing) = request(
        app,
        "POST",
        "/v1/tools/graph.node",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab", "id": "device:sw3", "graph_session_id": session})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");

    let (status, head) = request(
        app,
        "POST",
        "/v1/tools/graph.node",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab", "id": "device:sw3"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(head["graph_version"], 2);

    let (status, old) = request(
        app,
        "POST",
        "/v1/tools/graph.node",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab", "id": "device:sw3", "version": 1})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "version 1 must not contain sw3: {old}"
    );

    let (status, refreshed) = request(
        app,
        "POST",
        "/v1/tools/graph.session.refresh",
        Some("lab-reader"),
        Some(&json!({"graph_session_id": session})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{refreshed}");
    assert_eq!(refreshed["data"]["previous_version"], 1);
    assert_eq!(refreshed["data"]["snapshot_version"], 2);

    let (status, session_info) = request(
        app,
        "POST",
        "/v1/tools/graph.session.info",
        Some("lab-reader"),
        Some(&json!({"graph_session_id": session})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(session_info["data"]["state"], "active");

    let (status, listed_sessions) = request(
        app,
        "POST",
        "/v1/tools/graph.session.list",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab", "owner": "alice"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        listed_sessions["data"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let (status, readonly) = request(
        app,
        "POST",
        "/v1/tools/graph.upsert",
        Some("lab-writer"),
        Some(&json!({
            "namespace": "lab",
            "graph_session_id": session,
            "source": "fixture",
            "execution_id": "nope",
            "nodes": [{"id": "device:sw9", "type": "Device"}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(readonly["error"]["code"], "READ_ONLY_SESSION");

    let (status, reader_write) = request(
        app,
        "POST",
        "/v1/tools/graph.upsert",
        Some("lab-reader"),
        Some(&json!({
            "namespace": "lab",
            "source": "fixture",
            "execution_id": "nope",
            "nodes": [{"id": "device:sw9", "type": "Device"}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(reader_write["error"]["code"], "UNAUTHORIZED");

    let (status, closed) = request(
        app,
        "POST",
        "/v1/tools/graph.session.close",
        Some("lab-reader"),
        Some(&json!({"graph_session_id": session})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    let (status, expired) = request(
        app,
        "POST",
        "/v1/tools/graph.node",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab", "id": "device:sw1", "graph_session_id": session})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(expired["error"]["code"], "SESSION_EXPIRED");

    let (status, deactivated) = request(
        app,
        "POST",
        "/v1/tools/graph.deactivate",
        Some("lab-writer"),
        Some(&json!({
            "namespace": "lab",
            "ids": ["device:sw3"],
            "execution_id": "fixture-3"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{deactivated}");
    assert_eq!(deactivated["data"]["changed"], true);

    let (status, backed) = request(app, "POST", "/v1/admin/backup", Some("ops"), None).await;
    assert_eq!(status, StatusCode::OK, "{backed}");
    let (status, lag) = request(
        app,
        "POST",
        "/v1/tools/graph.backup.status",
        Some("lab-reader"),
        Some(&json!({"namespace": "lab"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{lag}");
    assert_eq!(lag["data"]["namespaces"][0]["lag_versions"], 0);

    let (status, instance) = request(
        app,
        "POST",
        "/v1/tools/graph.instance.info",
        Some("lab-reader"),
        Some(&json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(instance["data"]["role"], "primary");
    assert_eq!(instance["data"]["domain"], "test");

    let (status, metrics) = request(app, "GET", "/metrics", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let text = metrics.as_str().unwrap();
    assert!(text.contains("instance_id=\"ehgraph-test\""));
    assert!(text.contains("domain=\"test\""));
    assert!(text.contains("ehgraph_graph_version"));
    assert!(text.contains("ehgraph_backup_lag_versions"));

    let (status, rpc) = request(
        app,
        "POST",
        "/mcp",
        Some("lab-reader"),
        Some(&json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {"name": "graph.node", "arguments": {"namespace": "lab", "id": "site:s1"}}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rpc}");
    assert_eq!(rpc["id"], 7);
    let text = rpc["result"]["content"][0]["text"].as_str().unwrap();
    let envelope: Value = serde_json::from_str(text).unwrap();
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["id"], "site:s1");
}
