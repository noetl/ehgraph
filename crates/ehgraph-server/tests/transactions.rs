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

// ---------------------------------------------------------------------------
// Multi-statement transactions, conditional commits and serializable writes.
// Every test runs the real server stack: config, store on disk, router.

async fn tool_as(app: &axum::Router, token: &str, name: &str, body: Value) -> (StatusCode, Value) {
    request(
        app,
        "POST",
        &format!("/v1/tools/{name}"),
        Some(token),
        Some(&body),
    )
    .await
}

async fn tool(app: &axum::Router, name: &str, body: Value) -> (StatusCode, Value) {
    tool_as(app, "lab-writer", name, body).await
}

async fn cypher(app: &axum::Router, query: &str, params: Value) -> (StatusCode, Value) {
    tool(
        app,
        "graph.cypher",
        json!({"namespace": "lab", "query": query, "params": params}),
    )
    .await
}

async fn rows(app: &axum::Router, query: &str) -> Vec<Value> {
    let (status, out) = cypher(app, query, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{query}: {out}");
    out["data"]["rows"].as_array().unwrap().clone()
}

async fn head(app: &axum::Router) -> u64 {
    let (_, info) = tool(app, "graph.snapshot.info", json!({"namespace": "lab"})).await;
    info["graph_version"].as_u64().unwrap_or(0)
}

async fn begin(app: &axum::Router) -> String {
    let (status, out) = tool(app, "graph.tx.begin", json!({"namespace": "lab"})).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    out["data"]["tx_id"].as_str().unwrap().to_string()
}

async fn run(app: &axum::Router, tx: &str, query: &str) -> (StatusCode, Value) {
    tool(app, "graph.tx.run", json!({"tx_id": tx, "query": query})).await
}

async fn run_ok(app: &axum::Router, tx: &str, query: &str) -> Vec<Value> {
    let (status, out) = run(app, tx, query).await;
    assert_eq!(status, StatusCode::OK, "{query}: {out}");
    out["data"]["rows"].as_array().unwrap().clone()
}

async fn commit(app: &axum::Router, tx: &str) -> (StatusCode, Value) {
    tool(app, "graph.tx.commit", json!({"tx_id": tx})).await
}

async fn seed(app: &axum::Router) -> u64 {
    let (status, out) = tool(
        app,
        "graph.upsert",
        json!({"namespace": "lab", "source": "fixture", "execution_id": "seed",
        "nodes": [
            {"id": "acct:a", "type": "Account", "props": {"balance": 100}},
            {"id": "acct:b", "type": "Account", "props": {"balance": 50}},
            {"id": "gw:1", "type": "Gateway", "props": {"name": "gw"}}
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    head(app).await
}

#[tokio::test]
async fn a_transaction_spans_statements_and_commits_once() {
    let h = harness().await;
    let app = &h.app;
    let v0 = seed(app).await;

    let tx = begin(app).await;
    // Statement 1 writes; statement 2 sees it (read-your-writes inside the tx).
    run_ok(
        app,
        &tx,
        "MATCH (a:Account {id: 'acct:a'}) SET a.balance = a.balance - 30",
    )
    .await;
    run_ok(
        app,
        &tx,
        "MATCH (b:Account {id: 'acct:b'}) SET b.balance = b.balance + 30",
    )
    .await;
    run_ok(app, &tx, "CREATE (:Transfer {id: 'xfer:1', amount: 30})").await;
    let seen = run_ok(
        app,
        &tx,
        "MATCH (a:Account {id: 'acct:a'}), (b:Account {id: 'acct:b'}), (t:Transfer) \
         RETURN a.balance, b.balance, t.amount",
    )
    .await;
    assert_eq!(seen, vec![json!([70, 80, 30])]);

    // Isolation: nothing is visible outside the transaction before COMMIT.
    assert_eq!(head(app).await, v0);
    assert_eq!(
        rows(
            app,
            "MATCH (a:Account) RETURN a.id, a.balance ORDER BY a.id"
        )
        .await,
        vec![json!(["acct:a", 100]), json!(["acct:b", 50])]
    );
    assert!(rows(app, "MATCH (t:Transfer) RETURN t").await.is_empty());

    // COMMIT: all three statements land as exactly one graph version.
    let (status, out) = commit(app, &tx).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["data"]["graph_version"], v0 + 1);
    assert_eq!(out["data"]["writes"][0]["upserted"], 3);
    assert_eq!(head(app).await, v0 + 1);
    assert_eq!(
        rows(
            app,
            "MATCH (a:Account) RETURN a.id, a.balance ORDER BY a.id"
        )
        .await,
        vec![json!(["acct:a", 70]), json!(["acct:b", 80])]
    );
    // The transaction is closed.
    let (status, out) = run(app, &tx, "RETURN 1").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{out}");
}

#[tokio::test]
async fn rollback_and_failed_statements_write_nothing() {
    let h = harness().await;
    let app = &h.app;
    let v0 = seed(app).await;

    // ROLLBACK discards writes, creates and deletes alike.
    let tx = begin(app).await;
    run_ok(
        app,
        &tx,
        "MATCH (a:Account {id: 'acct:a'}) SET a.balance = 0",
    )
    .await;
    run_ok(app, &tx, "CREATE (:Account {id: 'acct:c', balance: 1})").await;
    run_ok(app, &tx, "MATCH (g:Gateway) DETACH DELETE g").await;
    let (status, out) = tool(app, "graph.tx.rollback", json!({"tx_id": tx})).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["data"]["rolled_back"], true);
    assert_eq!(head(app).await, v0);
    assert_eq!(
        rows(app, "MATCH (n) RETURN n.id ORDER BY n.id").await,
        vec![json!(["acct:a"]), json!(["acct:b"]), json!(["gw:1"])]
    );
    assert_eq!(
        rows(app, "MATCH (a {id: 'acct:a'}) RETURN a.balance").await,
        vec![json!([100])]
    );

    // A failing statement fails the whole transaction, including earlier writes.
    let tx = begin(app).await;
    run_ok(
        app,
        &tx,
        "MATCH (a:Account {id: 'acct:a'}) SET a.balance = 1",
    )
    .await;
    let (status, _) = run(
        app,
        &tx,
        "MATCH (a:Account {id: 'acct:a'}) SET a.balance = 1 / 0",
    )
    .await;
    assert_ne!(status, StatusCode::OK);
    let (status, out) = run(app, &tx, "RETURN 1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{out}");
    let (status, out) = commit(app, &tx).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{out}");
    assert!(out["error"]["message"]
        .as_str()
        .unwrap()
        .contains("failed earlier"));
    assert_eq!(head(app).await, v0);

    // A dangling DELETE (no DETACH) fails at the statement, nothing commits.
    let (_, out) = tool(
        app,
        "graph.upsert",
        json!({"namespace": "lab", "source": "fixture", "execution_id": "e",
               "edges": [{"src": "acct:a", "dst": "gw:1", "type": "VIA"}]}),
    )
    .await;
    let v1 = out["graph_version"].as_u64().unwrap();
    let tx = begin(app).await;
    run_ok(app, &tx, "CREATE (:Account {id: 'acct:z'})").await;
    let (status, _) = run(app, &tx, "MATCH (g:Gateway) DELETE g").await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = commit(app, &tx).await;
    assert_ne!(status, StatusCode::OK);
    assert_eq!(head(app).await, v1);
    assert!(rows(app, "MATCH (a {id: 'acct:z'}) RETURN a")
        .await
        .is_empty());
}

#[tokio::test]
async fn a_concurrent_write_to_what_a_transaction_read_is_a_conflict() {
    let h = harness().await;
    let app = &h.app;
    seed(app).await;

    // Lost update: tx reads a balance, someone else changes it, tx writes.
    let tx = begin(app).await;
    let read = run_ok(
        app,
        &tx,
        "MATCH (a:Account {id: 'acct:a'}) RETURN a.balance",
    )
    .await;
    assert_eq!(read, vec![json!([100])]);
    let (status, _) = cypher(
        app,
        "MATCH (a:Account {id: 'acct:a'}) SET a.balance = 999",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let after_other = head(app).await;
    run_ok(
        app,
        &tx,
        "MATCH (a:Account {id: 'acct:a'}) SET a.balance = 100 + 5",
    )
    .await;
    let (status, out) = commit(app, &tx).await;
    assert_eq!(status, StatusCode::CONFLICT, "{out}");
    assert_eq!(out["error"]["code"], "VERSION_CONFLICT");
    assert!(out["error"]["message"]
        .as_str()
        .unwrap()
        .contains("serialization conflict"));
    assert_eq!(head(app).await, after_other);
    assert_eq!(
        rows(app, "MATCH (a {id: 'acct:a'}) RETURN a.balance").await,
        vec![json!([999])]
    );

    // Phantom: tx counts Accounts, someone creates one, tx writes the count.
    let tx = begin(app).await;
    run_ok(
        app,
        &tx,
        "MATCH (a:Account) WITH count(a) AS n CREATE (:Summary {id: 's', accounts: n})",
    )
    .await;
    let (status, _) = cypher(
        app,
        "CREATE (:Account {id: 'acct:new', balance: 0})",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, out) = commit(app, &tx).await;
    assert_eq!(status, StatusCode::CONFLICT, "{out}");
    assert!(rows(app, "MATCH (s:Summary) RETURN s").await.is_empty());

    // Write skew through adjacency: tx reads gw's neighbours, someone adds an edge.
    let tx = begin(app).await;
    run_ok(app, &tx, "MATCH (g:Gateway {id: 'gw:1'}) OPTIONAL MATCH (g)-[r]->() WITH g, count(r) AS n SET g.links = n").await;
    let (status, out) = tool(
        app,
        "graph.upsert",
        json!({"namespace": "lab", "source": "fixture", "execution_id": "e",
               "edges": [{"src": "gw:1", "dst": "acct:b", "type": "SERVES"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let (status, _) = commit(app, &tx).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn unrelated_concurrent_writes_do_not_conflict() {
    let h = harness().await;
    let app = &h.app;
    seed(app).await;
    let tx = begin(app).await;
    run_ok(
        app,
        &tx,
        "MATCH (a:Account {id: 'acct:a'}) SET a.balance = a.balance + 1",
    )
    .await;
    // The topology writer keeps writing other elements meanwhile.
    for i in 0..3 {
        let (status, out) = tool(
            app,
            "graph.upsert",
            json!({"namespace": "lab", "source": "qnetmon", "execution_id": format!("cycle-{i}"),
                   "nodes": [{"id": format!("ap:{i}"), "type": "AccessPoint", "props": {"n": i}},
                             {"id": "gw:1", "type": "Gateway", "props": {"name": "gw", "cycle": i}}],
                   "edges": [{"src": format!("ap:{i}"), "dst": "gw:1", "type": "UPLINK"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{out}");
    }
    let before = head(app).await;
    let (status, out) = commit(app, &tx).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["data"]["graph_version"], before + 1);
    assert_eq!(
        rows(app, "MATCH (a {id: 'acct:a'}) RETURN a.balance").await,
        vec![json!([101])]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_increments_are_never_lost() {
    let h = harness().await;
    let app = h.app.clone();
    let (status, _) = cypher(&app, "CREATE (:Counter {id: 'c', n: 0})", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let v0 = head(&app).await;

    const WORKERS: usize = 8;
    const EACH: usize = 10;
    let mut handles = Vec::new();
    for w in 0..WORKERS {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            let mut conflicts = 0u32;
            for i in 0..EACH {
                loop {
                    // Odd workers use explicit two-statement transactions
                    // (read, then write the read value + 1); even workers use
                    // auto-commit (which retries internally a few times).
                    let committed = if w % 2 == 1 {
                        let tx = begin(&app).await;
                        let n = run_ok(&app, &tx, "MATCH (c:Counter {id: 'c'}) RETURN c.n").await[0][0]
                            .as_i64()
                            .unwrap();
                        run_ok(&app, &tx, &format!("MATCH (c:Counter {{id: 'c'}}) SET c.n = {}, c.last = '{w}-{i}'", n + 1)).await;
                        let (status, out) = commit(&app, &tx).await;
                        match status {
                            StatusCode::OK => true,
                            StatusCode::CONFLICT => {
                                assert_eq!(out["error"]["code"], "VERSION_CONFLICT");
                                false
                            }
                            other => panic!("{other}: {out}"),
                        }
                    } else {
                        let (status, out) = cypher(
                            &app,
                            &format!("MATCH (c:Counter {{id: 'c'}}) SET c.n = c.n + 1, c.last = '{w}-{i}'"),
                            json!({}),
                        )
                        .await;
                        match status {
                            StatusCode::OK => true,
                            StatusCode::CONFLICT => false,
                            other => panic!("{other}: {out}"),
                        }
                    };
                    if committed {
                        break;
                    }
                    conflicts += 1;
                }
            }
            conflicts
        }));
    }
    let mut conflicts = 0;
    for handle in handles {
        conflicts += handle.await.unwrap();
    }
    let total = (WORKERS * EACH) as i64;
    assert_eq!(
        rows(&app, "MATCH (c:Counter {id: 'c'}) RETURN c.n").await,
        vec![json!([total])],
        "{conflicts} conflicts were retried"
    );
    // Every increment is its own version: nothing was merged or lost.
    assert_eq!(head(&app).await, v0 + total as u64);
}

#[tokio::test]
async fn expect_makes_writes_conditional() {
    let h = harness().await;
    let app = &h.app;
    seed(app).await;
    let (_, node) = tool(
        app,
        "graph.node",
        json!({"namespace": "lab", "id": "acct:a"}),
    )
    .await;
    let v_a = node["data"]["version_updated"].as_u64().unwrap();
    let v0 = head(app).await;

    // graph.upsert with a stale element version: 409, nothing written.
    let upsert = |version: u64, balance: i64| {
        json!({"namespace": "lab", "source": "fixture", "execution_id": "x",
               "nodes": [{"id": "acct:a", "type": "Account", "props": {"balance": balance}}],
               "expect": {"versions": {"acct:a": version}}})
    };
    let (status, out) = tool(app, "graph.upsert", upsert(v_a + 1, 1)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{out}");
    assert_eq!(out["error"]["code"], "VERSION_CONFLICT");
    assert_eq!(head(app).await, v0);
    let (status, out) = tool(app, "graph.upsert", upsert(v_a, 1)).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let v1 = head(app).await;
    assert_eq!(v1, v0 + 1);
    // The old version no longer holds.
    let (status, _) = tool(app, "graph.upsert", upsert(v_a, 2)).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Create-if-absent with version 0.
    let create = json!({"namespace": "lab", "source": "fixture", "execution_id": "x",
        "nodes": [{"id": "lock:1", "type": "Lock", "props": {}}],
        "expect": {"versions": {"lock:1": 0}}});
    assert_eq!(
        tool(app, "graph.upsert", create.clone()).await.0,
        StatusCode::OK
    );
    assert_eq!(
        tool(app, "graph.upsert", create).await.0,
        StatusCode::CONFLICT
    );

    // graph.deactivate conditional on the graph version.
    let (status, _) = tool(
        app,
        "graph.deactivate",
        json!({"namespace": "lab", "execution_id": "x", "ids": ["lock:1"], "expect": {"graph_version": v1}}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let now = head(app).await;
    let (status, out) = tool(
        app,
        "graph.deactivate",
        json!({"namespace": "lab", "execution_id": "x", "ids": ["lock:1"], "expect": {"graph_version": now}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");

    // graph.cypher and graph.tx.commit take `expect` too.
    let (status, _) = tool(
        app,
        "graph.cypher",
        json!({"namespace": "lab", "query": "MATCH (b:Account {id: 'acct:b'}) SET b.balance = 0",
               "expect": {"versions": {"acct:b": 12345}}}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let tx = begin(app).await;
    run_ok(
        app,
        &tx,
        "MATCH (b:Account {id: 'acct:b'}) SET b.balance = 0",
    )
    .await;
    let (status, _) = tool(
        app,
        "graph.tx.commit",
        json!({"tx_id": tx, "expect": {"graph_version": 1}}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        rows(app, "MATCH (b {id: 'acct:b'}) RETURN b.balance").await,
        vec![json!([50])]
    );
    // An unknown expect field is rejected, never silently ignored.
    let (status, _) = tool(
        app,
        "graph.upsert",
        json!({"namespace": "lab", "source": "f", "execution_id": "x", "nodes": [],
               "expect": {"version": 3}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn autocommit_cypher_is_one_atomic_version() {
    let h = harness().await;
    let app = &h.app;
    seed(app).await;
    let (_, out) = tool(
        app,
        "graph.upsert",
        json!({"namespace": "lab", "source": "fixture", "execution_id": "e",
               "edges": [{"src": "acct:a", "dst": "gw:1", "type": "VIA"}]}),
    )
    .await;
    let v0 = out["graph_version"].as_u64().unwrap();
    // Create, update and delete in one statement: one version, not two.
    let (status, out) = cypher(
        app,
        "MATCH (g:Gateway {id: 'gw:1'}) DETACH DELETE g \
         WITH 1 AS one MATCH (a:Account {id: 'acct:a'}) SET a.balance = 1 \
         CREATE (:Gateway {id: 'gw:2'})",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["graph_version"], v0 + 1);
    assert_eq!(head(app).await, v0 + 1);
    assert_eq!(out["data"]["writes"].as_array().unwrap().len(), 1);
    assert_eq!(out["data"]["writes"][0]["deactivated"], 2);
}

#[tokio::test]
async fn transactions_are_private_and_expire() {
    let h = harness().await;
    let app = &h.app;
    seed(app).await;
    let tx = begin(app).await;
    // Another principal cannot use it.
    let (status, _) = tool_as(
        app,
        "ops",
        "graph.tx.run",
        json!({"tx_id": tx, "query": "RETURN 1"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A read-only principal can open a transaction but not write in it.
    let (status, out) = tool_as(
        app,
        "lab-reader",
        "graph.tx.begin",
        json!({"namespace": "lab"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let rtx = out["data"]["tx_id"].as_str().unwrap().to_string();
    let (status, _) = tool_as(
        app,
        "lab-reader",
        "graph.tx.run",
        json!({"tx_id": rtx, "query": "MATCH (n) RETURN count(n)"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = tool_as(
        app,
        "lab-reader",
        "graph.tx.run",
        json!({"tx_id": rtx, "query": "CREATE (:X {id: 'x'})"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Expiry.
    let (_, out) = tool(
        app,
        "graph.tx.begin",
        json!({"namespace": "lab", "timeout_ms": 1}),
    )
    .await;
    let short = out["data"]["tx_id"].as_str().unwrap().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let (status, out) = run(app, &short, "RETURN 1").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{out}");
    // Rolling back an unknown transaction is not an error.
    let (status, out) = tool(app, "graph.tx.rollback", json!({"tx_id": "tx-nope"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(out["data"]["rolled_back"], false);
}

#[tokio::test]
async fn neo4j_query_api_explicit_transactions() {
    let h = harness().await;
    let app = &h.app;
    let v0 = seed(app).await;
    let token = Some("lab-writer");

    // Open with a first statement, run another, commit with a last one.
    let (status, out) = request(
        app,
        "POST",
        "/db/lab/query/v2/tx",
        token,
        Some(&json!({"statement": "MATCH (a:Account {id: 'acct:a'}) SET a.balance = 10 RETURN a.balance"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{out}");
    assert_eq!(out["data"]["values"], json!([[10]]));
    let id = out["transaction"]["id"].as_str().unwrap().to_string();
    assert!(out["transaction"]["expires"].is_string(), "{out}");
    let (status, out) = request(
        app,
        "POST",
        &format!("/db/lab/query/v2/tx/{id}"),
        token,
        Some(&json!({"statement": "MATCH (a:Account {id: $id}) RETURN a.balance", "parameters": {"id": "acct:a"}})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{out}");
    assert_eq!(out["data"]["values"], json!([[10]]));
    assert_eq!(head(app).await, v0);
    let (status, out) = request(
        app,
        "POST",
        &format!("/db/lab/query/v2/tx/{id}/commit"),
        token,
        Some(&json!({"statement": "MATCH (b:Account {id: 'acct:b'}) SET b.balance = 20"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{out}");
    assert_eq!(out["bookmarks"], json!([format!("ehgraph:lab:{}", v0 + 1)]));
    assert_eq!(
        rows(app, "MATCH (a:Account) RETURN a.balance ORDER BY a.id").await,
        vec![json!([10]), json!([20])]
    );

    // Open, write, DELETE = rollback.
    let (_, out) = request(app, "POST", "/db/lab/query/v2/tx", token, None).await;
    let id = out["transaction"]["id"].as_str().unwrap().to_string();
    let (status, _) = request(
        app,
        "POST",
        &format!("/db/lab/query/v2/tx/{id}"),
        token,
        Some(&json!({"statement": "CREATE (:Account {id: 'acct:tmp'})"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _) = request(
        app,
        "DELETE",
        &format!("/db/lab/query/v2/tx/{id}"),
        token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(head(app).await, v0 + 1);
    let (status, out) = request(
        app,
        "DELETE",
        &format!("/db/lab/query/v2/tx/{id}"),
        token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        out["errors"][0]["code"],
        "Neo.ClientError.Transaction.TransactionNotFound"
    );

    // A lost race surfaces as a transient (driver-retryable) error.
    let (_, out) = request(
        app,
        "POST",
        "/db/lab/query/v2/tx",
        token,
        Some(
            &json!({"statement": "MATCH (a:Account {id: 'acct:a'}) SET a.balance = a.balance + 1"}),
        ),
    )
    .await;
    let id = out["transaction"]["id"].as_str().unwrap().to_string();
    cypher(
        app,
        "MATCH (a:Account {id: 'acct:a'}) SET a.balance = 0",
        json!({}),
    )
    .await;
    let (status, out) = request(
        app,
        "POST",
        &format!("/db/lab/query/v2/tx/{id}/commit"),
        token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{out}");
    assert_eq!(
        out["errors"][0]["code"],
        "Neo.TransientError.Transaction.Outdated"
    );
}
