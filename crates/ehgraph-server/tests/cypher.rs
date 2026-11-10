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
// openCypher conformance-style suite (docs/wiki/Cypher.md). Every test runs the
// real server stack — config, store on disk, router — with no mocks.

async fn cypher(
    app: &axum::Router,
    token: &str,
    query: &str,
    params: Value,
) -> (StatusCode, Value) {
    cypher_with(
        app,
        token,
        json!({"namespace": "lab", "query": query, "params": params}),
    )
    .await
}

async fn cypher_with(app: &axum::Router, token: &str, body: Value) -> (StatusCode, Value) {
    request(app, "POST", "/v1/cypher", Some(token), Some(&body)).await
}

/// Rows of a successful query.
async fn rows(app: &axum::Router, query: &str, params: Value) -> Vec<Value> {
    let (status, out) = cypher(app, "lab-writer", query, params).await;
    assert_eq!(status, StatusCode::OK, "{query}: {out}");
    out["data"]["rows"].as_array().unwrap().clone()
}

async fn tool(app: &axum::Router, name: &str, body: Value) -> (StatusCode, Value) {
    request(
        app,
        "POST",
        &format!("/v1/tools/{name}"),
        Some("lab-writer"),
        Some(&body),
    )
    .await
}

async fn head(app: &axum::Router) -> u64 {
    let (_, info) = tool(app, "graph.snapshot.info", json!({"namespace": "lab"})).await;
    info["graph_version"].as_u64().unwrap()
}

/// The topology shape qnetmon writes with `graph.upsert` (gateway, switch, APs,
/// clients), written through the existing op — Cypher must read it as-is.
async fn seed_topology(app: &axum::Router) {
    let (status, out) = tool(
        app,
        "graph.upsert",
        json!({
            "namespace": "lab", "source": "unifi", "execution_id": "cycle-1",
            "nodes": [
                {"id": "internet:wan", "type": "Internet", "props": {"name": "Internet", "health": "ok"}},
                {"id": "gw:1", "type": "Gateway", "props": {"name": "Dream Machine Pro", "state": 1, "health": "ok"}},
                {"id": "sw:1", "type": "Switch", "props": {"name": "USW Pro", "state": 1, "health": "ok"}},
                {"id": "ap:1", "type": "AccessPoint", "props": {"name": "U7 Pro A", "num_sta": 5, "health": "ok"}},
                {"id": "ap:2", "type": "AccessPoint", "props": {"name": "U7 Pro B", "num_sta": 0, "health": "ok"}},
                {"id": "client:1", "type": "Client", "props": {"hostname": "laptop", "signal": -60}},
                {"id": "client:2", "type": "Client", "props": {"hostname": "phone", "signal": -80}}
            ],
            "edges": [
                {"src": "gw:1", "dst": "internet:wan", "type": "WAN", "props": {}},
                {"src": "sw:1", "dst": "gw:1", "type": "UPLINK", "props": {"speed_mbps": 10000}},
                {"src": "ap:1", "dst": "sw:1", "type": "UPLINK", "props": {"speed_mbps": 2500}},
                {"src": "ap:2", "dst": "sw:1", "type": "UPLINK", "props": {"speed_mbps": 2500}},
                {"src": "client:1", "dst": "ap:1", "type": "CONNECTED_TO", "props": {}},
                {"src": "client:2", "dst": "ap:1", "type": "CONNECTED_TO", "props": {}}
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
}

#[tokio::test]
async fn cypher_reads_topology_written_by_graph_upsert() {
    let h = harness().await;
    let app = &h.app;
    seed_topology(app).await;

    // Label scan + property filter + ORDER BY.
    let got = rows(
        app,
        "MATCH (a:AccessPoint) WHERE a.num_sta > 0 RETURN a.id, a.name ORDER BY a.id",
        json!({}),
    )
    .await;
    assert_eq!(got, vec![json!(["ap:1", "U7 Pro A"])]);

    // Which devices uplink directly to the gateway (incoming single hop).
    let got = rows(
        app,
        "MATCH (d)-[:UPLINK]->(g:Gateway) RETURN labels(d)[0] AS kind, d.id AS id",
        json!({}),
    )
    .await;
    assert_eq!(got, vec![json!(["Switch", "sw:1"])]);

    // Variable-length: every AP's path to the gateway, with hop count and node ids.
    let got = rows(
        app,
        "MATCH p = (a:AccessPoint)-[:UPLINK*1..4]->(g:Gateway) \
         RETURN a.id AS ap, length(p) AS hops, [n IN nodes(p) | n.id] AS via ORDER BY ap",
        json!({}),
    )
    .await;
    assert_eq!(
        got,
        vec![
            json!(["ap:1", 2, ["ap:1", "sw:1", "gw:1"]]),
            json!(["ap:2", 2, ["ap:2", "sw:1", "gw:1"]])
        ]
    );

    // Exact length and bounds: *1 does not reach the gateway from an AP, *2 does.
    assert!(rows(
        app,
        "MATCH (:AccessPoint {id:'ap:1'})-[:UPLINK*1]->(g:Gateway) RETURN g",
        json!({})
    )
    .await
    .is_empty());
    assert_eq!(
        rows(
            app,
            "MATCH (:AccessPoint {id:'ap:1'})-[:UPLINK*2]->(g:Gateway) RETURN g.id",
            json!({})
        )
        .await,
        vec![json!(["gw:1"])]
    );

    // Mixed types, any direction, shortestPath to the internet.
    let got = rows(
        app,
        "MATCH (c:Client {id: $client}), (i:Internet) \
         MATCH p = shortestPath((c)-[*..10]->(i)) RETURN [r IN relationships(p) | type(r)] AS hops",
        json!({"client": "client:1"}),
    )
    .await;
    assert_eq!(
        got,
        vec![json!([["CONNECTED_TO", "UPLINK", "UPLINK", "WAN"]])]
    );

    // Undirected neighbourhood of the switch; aggregation and DISTINCT.
    let got = rows(
        app,
        "MATCH (:Switch)-[r]-(n) RETURN type(r) AS t, count(DISTINCT n) AS c ORDER BY t",
        json!({}),
    )
    .await;
    assert_eq!(got, vec![json!(["UPLINK", 3])]);

    // Clients per AP with OPTIONAL MATCH (ap:2 has none), collect + size.
    let got = rows(
        app,
        "MATCH (a:AccessPoint) OPTIONAL MATCH (c:Client)-[:CONNECTED_TO]->(a) \
         WITH a, collect(c.hostname) AS hosts RETURN a.id, size(hosts), hosts ORDER BY a.id",
        json!({}),
    )
    .await;
    assert_eq!(
        got,
        vec![
            json!(["ap:1", 2, ["laptop", "phone"]]),
            json!(["ap:2", 0, []])
        ]
    );

    // Elements come back in the Query-API shape.
    let got = rows(
        app,
        "MATCH (s:Switch)-[r:UPLINK]->(g) RETURN s, r",
        json!({}),
    )
    .await;
    assert_eq!(got[0][0]["elementId"], "sw:1");
    assert_eq!(got[0][0]["labels"], json!(["Switch"]));
    assert_eq!(got[0][0]["properties"]["name"], "USW Pro");
    assert_eq!(got[0][1]["type"], "UPLINK");
    assert_eq!(got[0][1]["startNodeElementId"], "sw:1");
    assert_eq!(got[0][1]["endNodeElementId"], "gw:1");
    assert_eq!(got[0][1]["properties"]["speed_mbps"], 10000);

    // CASE, string predicates, IN, UNWIND, SKIP/LIMIT, WITH ... WHERE.
    let got = rows(
        app,
        "MATCH (c:Client) RETURN c.hostname AS h, CASE WHEN c.signal < -78 THEN 'weak' ELSE 'ok' END AS rf ORDER BY h",
        json!({}),
    )
    .await;
    assert_eq!(got, vec![json!(["laptop", "ok"]), json!(["phone", "weak"])]);
    let got = rows(
        app,
        "MATCH (n) WHERE n.id STARTS WITH 'ap:' AND n.name CONTAINS 'B' RETURN n.id",
        json!({}),
    )
    .await;
    assert_eq!(got, vec![json!(["ap:2"])]);
    let got = rows(
        app,
        "UNWIND [3, 1, 2, 1] AS x WITH DISTINCT x WHERE x > 1 RETURN x ORDER BY x DESC",
        json!({}),
    )
    .await;
    assert_eq!(got, vec![json!([3]), json!([2])]);
    let got = rows(
        app,
        "MATCH (n) RETURN n.id ORDER BY n.id SKIP 1 LIMIT 2",
        json!({}),
    )
    .await;
    assert_eq!(got, vec![json!(["ap:2"]), json!(["client:1"])]);
}

#[tokio::test]
async fn cypher_writes_round_trip_through_existing_ops() {
    let h = harness().await;
    let app = &h.app;

    let (status, out) = cypher(
        app,
        "lab-writer",
        "CREATE (q:Qpu {id: 'qpu:forte', name: 'Forte', qubits: 36}), \
                (s:Simulator {id: 'sim:ideal'}), \
                (j:Job {id: 'job:1', shots: 1024}), \
                (j)-[:RUNS_ON {queued_ms: 12}]->(q) RETURN q.name, j.shots",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["data"]["rows"], json!([["Forte", 1024]]));
    assert_eq!(out["data"]["stats"]["nodes_created"], 3);
    assert_eq!(out["data"]["stats"]["relationships_created"], 1);
    // All upserts commit as one version through graph.upsert.
    assert_eq!(out["data"]["writes"].as_array().unwrap().len(), 1);
    assert_eq!(out["data"]["writes"][0]["op"], "graph.upsert");
    let v1 = out["graph_version"].as_u64().unwrap();
    assert_eq!(head(app).await, v1);

    // The existing ops see exactly what Cypher wrote.
    let (_, node) = tool(
        app,
        "graph.node",
        json!({"namespace": "lab", "id": "qpu:forte"}),
    )
    .await;
    assert_eq!(node["data"]["type"], "Qpu");
    assert_eq!(
        node["data"]["props"],
        json!({"name": "Forte", "qubits": 36})
    );
    assert_eq!(node["data"]["source"], "cypher");
    assert!(
        node["data"]["props"].get("id").is_none(),
        "id is the element id, not a property"
    );
    let (_, nb) = tool(
        app,
        "graph.neighbors",
        json!({"namespace": "lab", "node_id": "job:1"}),
    )
    .await;
    assert_eq!(
        nb["data"]["edges"][0]["id"],
        "edge:job:1:RUNS_ON:qpu:forte:"
    );
    assert_eq!(nb["data"]["edges"][0]["props"]["queued_ms"], 12);
    let (_, q) = tool(
        app,
        "graph.query",
        json!({"namespace": "lab", "type": "Job"}),
    )
    .await;
    assert_eq!(q["data"]["nodes"].as_array().unwrap().len(), 1);

    // A CREATE without an id gets a generated, label-prefixed one.
    let got = rows(
        app,
        "CREATE (n:Note {text: 'hi'}) RETURN id(n) AS id",
        json!({}),
    )
    .await;
    assert!(got[0][0].as_str().unwrap().starts_with("note:"));

    // CREATE never silently duplicates: an existing id is a constraint error (409).
    let (status, out) = cypher(
        app,
        "lab-writer",
        "CREATE (:Qpu {id: 'qpu:forte'})",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{out}");
    assert!(out["error"]["message"]
        .as_str()
        .unwrap()
        .contains("use MERGE"));
}

#[tokio::test]
async fn merge_is_idempotent_and_set_remove_edit_properties() {
    let h = harness().await;
    let app = &h.app;
    // The batch upsert idiom: UNWIND rows, MERGE by id, SET the rest.
    let cycle = "UNWIND $devices AS d \
                 MERGE (n:Device {id: d.id}) SET n += d.props \
                 WITH n, d MATCH (g:Gateway {id: 'gw:1'}) MERGE (n)-[:UPLINK]->(g)";
    let params = json!({"devices": [
        {"id": "dev:1", "props": {"name": "a", "latency_ms": 3}},
        {"id": "dev:2", "props": {"name": "b", "latency_ms": 7}}
    ]});
    let (status, out) = cypher(
        app,
        "lab-writer",
        "MERGE (g:Gateway {id: 'gw:1'}) ON CREATE SET g.name = 'gw'",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let (status, first) = cypher(app, "lab-writer", cycle, params.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["data"]["stats"]["nodes_created"], 2);
    assert_eq!(first["data"]["stats"]["relationships_created"], 2);
    let v = head(app).await;

    // Same input again: nothing changes, no new version, no write issued.
    let (_, second) = cypher(app, "lab-writer", cycle, params.clone()).await;
    assert_eq!(
        second["data"]["stats"]["contains_updates"], false,
        "{second}"
    );
    assert_eq!(second["data"]["writes"], json!([]));
    assert_eq!(head(app).await, v);
    let (_, again) = cypher(
        app,
        "lab-writer",
        "MERGE (g:Gateway {id: 'gw:1'}) ON CREATE SET g.name = 'other'",
        json!({}),
    )
    .await;
    assert_eq!(again["data"]["writes"], json!([]));
    assert_eq!(
        rows(app, "MATCH (n) RETURN count(n)", json!({})).await,
        vec![json!([3])]
    );

    // ON MATCH SET runs only on a match.
    let got = rows(app, "MERGE (n:Device {id: 'dev:1'}) ON CREATE SET n.new = true ON MATCH SET n.seen = 2 RETURN n.new, n.seen", json!({})).await;
    assert_eq!(got, vec![json!([null, 2])]);

    // SET n.k, SET n += map (null removes), REMOVE n.k, SET n = map.
    rows(
        app,
        "MATCH (n {id: 'dev:1'}) SET n.status = 'offline', n.latency_ms = null",
        json!({}),
    )
    .await;
    let (_, node) = tool(
        app,
        "graph.node",
        json!({"namespace": "lab", "id": "dev:1"}),
    )
    .await;
    assert_eq!(
        node["data"]["props"],
        json!({"name": "a", "seen": 2, "status": "offline"})
    );
    rows(
        app,
        "MATCH (n {id: 'dev:1'}) SET n += {status: 'online', name: null} REMOVE n.seen",
        json!({}),
    )
    .await;
    let (_, node) = tool(
        app,
        "graph.node",
        json!({"namespace": "lab", "id": "dev:1"}),
    )
    .await;
    assert_eq!(node["data"]["props"], json!({"status": "online"}));
    rows(
        app,
        "MATCH (n {id: 'dev:1'}) SET n = {id: 'dev:1', name: 'fresh'}",
        json!({}),
    )
    .await;
    let (_, node) = tool(
        app,
        "graph.node",
        json!({"namespace": "lab", "id": "dev:1"}),
    )
    .await;
    assert_eq!(node["data"]["props"], json!({"name": "fresh"}));
    // Relationship properties too.
    rows(
        app,
        "MATCH (:Device {id: 'dev:2'})-[r:UPLINK]->() SET r.speed = 1000",
        json!({}),
    )
    .await;
    assert_eq!(
        rows(
            app,
            "MATCH ()-[r:UPLINK {speed: 1000}]->() RETURN count(r)",
            json!({})
        )
        .await,
        vec![json!([1])]
    );

    // The element id is immutable; a second label is out of model.
    let (status, _) = cypher(
        app,
        "lab-writer",
        "MATCH (n {id: 'dev:1'}) SET n.id = 'x'",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = cypher(
        app,
        "lab-writer",
        "MATCH (n {id: 'dev:1'}) SET n:Other",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = cypher(
        app,
        "lab-writer",
        "CREATE (:A:B {id: 'two-labels'})",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn delete_is_soft_deactivation() {
    let h = harness().await;
    let app = &h.app;
    seed_topology(app).await;

    // A plain DELETE of a connected node fails like Neo4j (409), and writes nothing.
    let before = head(app).await;
    let (status, out) = cypher(
        app,
        "lab-writer",
        "MATCH (a:AccessPoint {id: 'ap:2'}) DELETE a",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{out}");
    assert_eq!(head(app).await, before);

    // Deleting the relationship first, in the same query, is fine.
    let (status, out) = cypher(
        app,
        "lab-writer",
        "MATCH (a:AccessPoint {id: 'ap:2'})-[r]->() DELETE r, a",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["data"]["writes"][0]["op"], "graph.deactivate");
    assert_eq!(out["data"]["stats"]["nodes_deleted"], 1);

    // DETACH DELETE takes the incident edges with it.
    let (_, out) = cypher(
        app,
        "lab-writer",
        "MATCH (a:AccessPoint {id: 'ap:1'}) DETACH DELETE a",
        json!({}),
    )
    .await;
    assert_eq!(out["data"]["stats"]["relationships_deleted"], 3, "{out}");

    // Soft: the element is kept, inactive — exactly what graph.deactivate does.
    let (_, node) = tool(app, "graph.node", json!({"namespace": "lab", "id": "ap:1"})).await;
    assert_eq!(node["data"]["active"], false);
    assert!(rows(app, "MATCH (a:AccessPoint) RETURN a", json!({}))
        .await
        .is_empty());
    // `active: false` is the offline view (same filter as graph.query).
    let (_, out) = cypher_with(
        app,
        "lab-writer",
        json!({"namespace": "lab", "active": false, "query": "MATCH (n:AccessPoint) RETURN n.id, eh.active(n) ORDER BY n.id"}),
    )
    .await;
    assert_eq!(
        out["data"]["rows"],
        json!([["ap:1", false], ["ap:2", false]])
    );
    // An updating query cannot run over inactive elements.
    let (status, _) = cypher_with(
        app,
        "lab-writer",
        json!({"namespace": "lab", "active": "any", "query": "MATCH (n) SET n.x = 1"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Coming back online: MERGE by id reactivates the same element (history kept).
    rows(app, "MERGE (a:AccessPoint {id: 'ap:1'}) SET a.health = 'ok' WITH a MATCH (s:Switch {id: 'sw:1'}) MERGE (a)-[:UPLINK]->(s)", json!({})).await;
    let (_, node) = tool(app, "graph.node", json!({"namespace": "lab", "id": "ap:1"})).await;
    assert_eq!(node["data"]["active"], true);
    assert_eq!(node["data"]["version_created"], 1);
    let got = rows(
        app,
        "MATCH p = (:AccessPoint {id: 'ap:1'})-[:UPLINK*]->(:Gateway) RETURN length(p)",
        json!({}),
    )
    .await;
    assert_eq!(got, vec![json!([2])]);
    // The existing path op agrees.
    let (_, path) = tool(
        app,
        "graph.path",
        json!({"namespace": "lab", "src": "ap:1", "dst": "internet:wan"}),
    )
    .await;
    assert_eq!(
        path["data"]["nodes"],
        json!(["ap:1", "sw:1", "gw:1", "internet:wan"])
    );
}

#[tokio::test]
async fn cypher_permissions_errors_and_sessions() {
    let h = harness().await;
    let app = &h.app;
    seed_topology(app).await;

    // Readers can read, not write.
    let (status, _) = cypher(app, "lab-reader", "MATCH (n) RETURN count(n)", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, out) = cypher(app, "lab-reader", "CREATE (:X {id: 'x'})", json!({})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{out}");

    // Syntax errors carry a position; unsupported clauses say so.
    let (status, out) = cypher(app, "lab-writer", "MATCH (n RETURN n", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        out["error"]["message"].as_str().unwrap().contains("line 1"),
        "{out}"
    );
    let (_, out) = cypher(
        app,
        "lab-writer",
        "MATCH (n) RETURN n UNION MATCH (m) RETURN m",
        json!({}),
    )
    .await;
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not supported"),
        "{out}"
    );
    let (_, out) = cypher(
        app,
        "lab-writer",
        "MATCH (n) WHERE n.id = $missing RETURN n",
        json!({}),
    )
    .await;
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("$missing"),
        "{out}"
    );

    // Reads may pin a version or a session; writes may not.
    let v = head(app).await;
    rows(app, "CREATE (:Marker {id: 'm'})", json!({})).await;
    let (_, out) = cypher_with(
        app,
        "lab-writer",
        json!({"namespace": "lab", "version": v, "query": "MATCH (m:Marker) RETURN count(m)"}),
    )
    .await;
    assert_eq!(out["data"]["rows"], json!([[0]]), "{out}");
    let (_, opened) = tool(
        app,
        "graph.session.open",
        json!({"namespace": "lab", "owner": "test"}),
    )
    .await;
    let session = opened["data"]["graph_session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, out) = cypher_with(
        app,
        "lab-writer",
        json!({"namespace": "lab", "graph_session_id": session, "query": "MATCH (a:AccessPoint) RETURN count(a)"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["data"]["rows"], json!([[2]]));
    let (status, _) = cypher_with(
        app,
        "lab-writer",
        json!({"namespace": "lab", "graph_session_id": session, "query": "CREATE (:X {id: 'y'})"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Unbounded expansion is capped, not unbounded.
    let (status, out) = cypher(
        app,
        "lab-writer",
        "MATCH (a)-[*1..99]->(b) RETURN count(*)",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{out}");
}

#[tokio::test]
async fn neo4j_query_api_shape_and_mcp_tool() {
    let h = harness().await;
    let app = &h.app;
    seed_topology(app).await;

    // Neo4j Query API v2 shape: POST /db/{database}/query/v2 → 202.
    let (status, out) = request(
        app,
        "POST",
        "/db/lab/query/v2",
        Some("lab-writer"),
        Some(&json!({"statement": "MATCH (g:Gateway) RETURN g.name AS name, $x AS x", "parameters": {"x": 7}})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{out}");
    assert_eq!(out["data"]["fields"], json!(["name", "x"]));
    assert_eq!(out["data"]["values"], json!([["Dream Machine Pro", 7]]));
    assert_eq!(out["counters"]["containsUpdates"], false);
    let (status, out) = request(
        app,
        "POST",
        "/db/lab/query/v2",
        Some("lab-writer"),
        Some(&json!({"statement": "MERGE (n:Probe {id: 'p1'}) SET n.ok = true"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(out["counters"]["nodesCreated"], 1);
    assert_eq!(out["counters"]["propertiesSet"], 1);
    let (status, out) = request(
        app,
        "POST",
        "/db/lab/query/v2",
        Some("lab-writer"),
        Some(&json!({"statement": "MATC (n) RETURN n"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        out["errors"][0]["code"],
        "Neo.ClientError.Statement.SyntaxError"
    );

    // MCP: listed and callable like every other tool.
    let (_, listed) = request(app, "GET", "/mcp/tools", None, None).await;
    assert!(listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "graph.cypher"));
    let (status, rpc) = request(
        app,
        "POST",
        "/mcp",
        Some("lab-writer"),
        Some(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
            "name": "graph.cypher",
            "arguments": {"namespace": "lab", "query": "MATCH (c:Client)-[:CONNECTED_TO]->(a) RETURN a.id, count(c)"}
        }})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rpc["result"]["isError"], false, "{rpc}");
    let text: Value =
        serde_json::from_str(rpc["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["data"]["rows"], json!([["ap:1", 2]]));
}
