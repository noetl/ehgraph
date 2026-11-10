//! MCP JSON-RPC and HTTP routes over one [`EhGraph`].
//!
//! The same router serves both modes. The standalone server binds it to its own
//! listener. An embedded host mounts it under a prefix on its listener, e.g.
//! `host_router.nest("/ehgraph/network", graph.router())`.
//!
//! Engine calls do fsync'd I/O, so every call runs on the blocking pool. In
//! embedded mode the runtime belongs to the host, and a stalled worker thread there
//! stalls the host's own requests.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use ehgraph_core::{EhError, Envelope};
use serde_json::{json, Value};

use crate::EhGraph;

pub fn router(graph: EhGraph) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics_route))
        .route("/mcp", post(mcp))
        .route("/mcp/tools", get(tools_list))
        .route("/v1/tools/{tool}", post(http_tool))
        .route("/v1/cypher", post(cypher_route))
        .route("/db/{namespace}/query/v2", post(neo4j_query))
        .route("/db/{namespace}/query/v2/tx", post(neo4j_tx_open))
        .route("/db/{namespace}/query/v2/tx/{tx_id}", post(neo4j_tx_run))
        .route(
            "/db/{namespace}/query/v2/tx/{tx_id}",
            delete(neo4j_tx_rollback),
        )
        .route(
            "/db/{namespace}/query/v2/tx/{tx_id}/commit",
            post(neo4j_tx_commit),
        )
        .route("/v1/admin/backup", post(admin_backup))
        .with_state(graph)
}

fn auth_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

async fn healthz(State(graph): State<EhGraph>) -> Response {
    let identity = graph.identity();
    let serving = graph.is_serving();
    let body = Json(json!({
        "instance_id": identity.instance_id,
        "domain": identity.domain,
        "role": identity.role.as_str(),
        "mode": graph.mode().as_str(),
        "state": if serving { "serving" } else { "shut_down" },
    }));
    if serving {
        body.into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, body).into_response()
    }
}

async fn metrics_route(State(graph): State<EhGraph>) -> Response {
    let text = blocking(move || graph.metrics_text())
        .await
        .unwrap_or_else(|err| format!("# {err}\n"));
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        text,
    )
        .into_response()
}

async fn tools_list(State(graph): State<EhGraph>) -> Json<Value> {
    Json(json!({"tools": tool_json(&graph)}))
}

async fn http_tool(
    State(graph): State<EhGraph>,
    Path(tool): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let header = auth_header(&headers);
    let args = if body.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        match serde_json::from_str::<Value>(&body) {
            Ok(value) => value,
            Err(err) => {
                return envelope_response(
                    &graph,
                    Err(EhError::invalid(format!("request json: {err}"))),
                )
            }
        }
    };
    let g = graph.clone();
    let envelope = blocking(move || g.call_with_header(header.as_deref(), &tool, &args)).await;
    envelope_response(&graph, envelope)
}

/// `POST /v1/cypher` — the `graph.cypher` tool, same body and envelope.
async fn cypher_route(State(graph): State<EhGraph>, headers: HeaderMap, body: String) -> Response {
    http_tool(
        State(graph),
        Path("graph.cypher".to_string()),
        headers,
        body,
    )
    .await
}

/// `POST /db/{namespace}/query/v2` — shaped like the Neo4j Query API v2
/// (`{"statement", "parameters"}` in; `{"data": {"fields", "values"}, "counters"}`
/// out, `202 Accepted`), so HTTP Cypher clients written for it work against a
/// namespace. The database name is the ehgraph namespace. Auth is the same bearer
/// token as every other route.
async fn neo4j_query(
    State(graph): State<EhGraph>,
    Path(namespace): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let request = match neo4j_body(&body, true) {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let statement = request["statement"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let args = json!({
        "namespace": namespace,
        "query": statement,
        "params": request.get("parameters").cloned().unwrap_or(json!({})),
    });
    let envelope = neo4j_call(&graph, &headers, "graph.cypher", args).await;
    if !envelope.ok {
        return neo4j_failure(&envelope);
    }
    let version = envelope.graph_version.unwrap_or(0);
    let mut out = neo4j_result(envelope.data.as_ref().unwrap_or(&Value::Null));
    out["bookmarks"] = json!([format!("ehgraph:{namespace}:{version}")]);
    (StatusCode::ACCEPTED, Json(out)).into_response()
}

/// `POST /db/{namespace}/query/v2/tx` — open an explicit transaction, optionally
/// running a first statement in it. Like Neo4j's Query API, the response carries
/// `transaction: {id, expires}`.
async fn neo4j_tx_open(
    State(graph): State<EhGraph>,
    Path(namespace): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let request = match neo4j_body(&body, false) {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let mut begin = json!({"namespace": namespace});
    if let Some(ms) = request.get("timeout_ms").and_then(Value::as_i64) {
        begin["timeout_ms"] = json!(ms);
    }
    let envelope = neo4j_call(&graph, &headers, "graph.tx.begin", begin).await;
    if !envelope.ok {
        return neo4j_failure(&envelope);
    }
    let info = envelope.data.unwrap_or(Value::Null);
    let tx_id = info["tx_id"].as_str().unwrap_or_default().to_string();
    let mut out = json!({});
    if request.get("statement").and_then(Value::as_str).is_some() {
        let run = neo4j_tx_statement(&graph, &headers, &tx_id, &request).await;
        if !run.ok {
            return neo4j_failure(&run);
        }
        out = neo4j_result(run.data.as_ref().unwrap_or(&Value::Null));
    }
    out["transaction"] = neo4j_tx_json(&graph, &tx_id, info["expires_ms"].as_i64());
    (StatusCode::ACCEPTED, Json(out)).into_response()
}

/// `POST /db/{namespace}/query/v2/tx/{tx_id}` — run a statement in the transaction.
async fn neo4j_tx_run(
    State(graph): State<EhGraph>,
    Path((_namespace, tx_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let request = match neo4j_body(&body, true) {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let run = neo4j_tx_statement(&graph, &headers, &tx_id, &request).await;
    if !run.ok {
        return neo4j_failure(&run);
    }
    let data = run.data.unwrap_or(Value::Null);
    let mut out = neo4j_result(&data);
    out["transaction"] = neo4j_tx_json(&graph, &tx_id, data["expires_ms"].as_i64());
    (StatusCode::ACCEPTED, Json(out)).into_response()
}

/// `POST /db/{namespace}/query/v2/tx/{tx_id}/commit` — optionally run a last
/// statement, then commit. ehgraph extension: `"expect": {graph_version, versions}`.
async fn neo4j_tx_commit(
    State(graph): State<EhGraph>,
    Path((namespace, tx_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let request = match neo4j_body(&body, false) {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let mut out = json!({});
    if request.get("statement").and_then(Value::as_str).is_some() {
        let run = neo4j_tx_statement(&graph, &headers, &tx_id, &request).await;
        if !run.ok {
            // The statement failed the transaction; release it.
            let _ = neo4j_call(
                &graph,
                &headers,
                "graph.tx.rollback",
                json!({"tx_id": tx_id}),
            )
            .await;
            return neo4j_failure(&run);
        }
        out = neo4j_result(run.data.as_ref().unwrap_or(&Value::Null));
    }
    let mut commit = json!({"tx_id": tx_id});
    if let Some(expect) = request.get("expect") {
        commit["expect"] = expect.clone();
    }
    let done = neo4j_call(&graph, &headers, "graph.tx.commit", commit).await;
    if !done.ok {
        return neo4j_failure(&done);
    }
    let version = done.graph_version.unwrap_or(0);
    out["bookmarks"] = json!([format!("ehgraph:{namespace}:{version}")]);
    (StatusCode::ACCEPTED, Json(out)).into_response()
}

/// `DELETE /db/{namespace}/query/v2/tx/{tx_id}` — roll back.
async fn neo4j_tx_rollback(
    State(graph): State<EhGraph>,
    Path((_namespace, tx_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let done = neo4j_call(
        &graph,
        &headers,
        "graph.tx.rollback",
        json!({"tx_id": tx_id}),
    )
    .await;
    if !done.ok {
        return neo4j_failure(&done);
    }
    let gone = !done
        .data
        .as_ref()
        .and_then(|d| d.get("rolled_back"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if gone {
        return neo4j_error(
            StatusCode::NOT_FOUND,
            "Neo.ClientError.Transaction.TransactionNotFound",
            &format!("transaction {tx_id} not found (committed, rolled back, or expired)"),
        );
    }
    (StatusCode::ACCEPTED, Json(json!({}))).into_response()
}

/// Parse a Query API body; `statement` may be required.
fn neo4j_body(body: &str, statement_required: bool) -> Result<Value, Box<Response>> {
    let request: Value = if body.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(body).map_err(|err| {
            Box::new(neo4j_error(
                StatusCode::BAD_REQUEST,
                "Neo.ClientError.Request.Invalid",
                &format!("request json: {err}"),
            ))
        })?
    };
    if statement_required && request.get("statement").and_then(Value::as_str).is_none() {
        return Err(Box::new(neo4j_error(
            StatusCode::BAD_REQUEST,
            "Neo.ClientError.Request.Invalid",
            "`statement` is required",
        )));
    }
    Ok(request)
}

async fn neo4j_call(
    graph: &EhGraph,
    headers: &HeaderMap,
    tool: &'static str,
    args: Value,
) -> Envelope {
    let header = auth_header(headers);
    let g = graph.clone();
    or_failure(
        graph,
        blocking(move || g.call_with_header(header.as_deref(), tool, &args)).await,
    )
}

async fn neo4j_tx_statement(
    graph: &EhGraph,
    headers: &HeaderMap,
    tx_id: &str,
    request: &Value,
) -> Envelope {
    let args = json!({
        "tx_id": tx_id,
        "query": request["statement"].as_str().unwrap_or_default(),
        "params": request.get("parameters").cloned().unwrap_or(json!({})),
    });
    neo4j_call(graph, headers, "graph.tx.run", args).await
}

fn neo4j_tx_json(graph: &EhGraph, tx_id: &str, expires_ms: Option<i64>) -> Value {
    let expires_ms = expires_ms.or_else(|| graph.tx_expires(tx_id));
    let expires = expires_ms
        .map(|ms| ehgraph_core::PropValue::Timestamp(ms).to_json()["$timestamp"].clone())
        .unwrap_or(Value::Null);
    json!({"id": tx_id, "expires": expires})
}

/// A `graph.cypher` / `graph.tx.run` result in the Query API's shape.
fn neo4j_result(data: &Value) -> Value {
    let stats = &data["stats"];
    let n = |key: &str| stats.get(key).and_then(Value::as_u64).unwrap_or(0);
    json!({
        "data": {"fields": data["columns"], "values": data["rows"]},
        "counters": {
            "containsUpdates": stats.get("contains_updates").and_then(Value::as_bool).unwrap_or(false),
            "nodesCreated": n("nodes_created"),
            "nodesDeleted": n("nodes_deleted"),
            "propertiesSet": n("properties_set"),
            "relationshipsCreated": n("relationships_created"),
            "relationshipsDeleted": n("relationships_deleted"),
            "labelsAdded": 0,
            "labelsRemoved": 0,
            "indexesAdded": 0,
            "indexesRemoved": 0,
            "constraintsAdded": 0,
            "constraintsRemoved": 0,
            "containsSystemUpdates": false,
            "systemUpdates": 0,
        },
    })
}

/// A failed envelope as a Query API error response.
fn neo4j_failure(envelope: &Envelope) -> Response {
    let err = envelope.error.as_ref();
    let message = err.map(|e| e.message.clone()).unwrap_or_default();
    let code = match ehgraph_cypher::kind_of_message(&message) {
        Some(kind) => kind.neo4j_code(),
        None => match err.map(|e| e.code) {
            Some(ehgraph_core::ErrorCode::Unauthorized) => "Neo.ClientError.Security.Unauthorized",
            Some(ehgraph_core::ErrorCode::NotFound) if message.starts_with("transaction ") => {
                "Neo.ClientError.Transaction.TransactionNotFound"
            }
            Some(ehgraph_core::ErrorCode::NotFound) => "Neo.ClientError.Database.DatabaseNotFound",
            Some(ehgraph_core::ErrorCode::VersionConflict)
                if message.starts_with("serialization conflict") =>
            {
                "Neo.TransientError.Transaction.Outdated"
            }
            Some(ehgraph_core::ErrorCode::VersionConflict | ehgraph_core::ErrorCode::Conflict) => {
                "Neo.ClientError.Schema.ConstraintValidationFailed"
            }
            _ => "Neo.ClientError.Statement.ArgumentError",
        },
    };
    let status = StatusCode::from_u16(err.map(|e| e.code.http_status()).unwrap_or(400))
        .unwrap_or(StatusCode::BAD_REQUEST);
    neo4j_error(status, code, &message)
}

fn neo4j_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"errors": [{"code": code, "message": message}]})),
    )
        .into_response()
}

async fn admin_backup(State(graph): State<EhGraph>, headers: HeaderMap) -> Response {
    let header = auth_header(&headers);
    let g = graph.clone();
    let result = blocking(move || {
        let caller = g.authorize_header(header.as_deref())?;
        if !caller.write_all {
            return Err(EhError::unauthorized(
                "backup requires an operator grant (namespace \"*\")",
            ));
        }
        g.backup_now()
    })
    .await
    .and_then(|inner| inner);
    match result {
        Ok(checkpoint) => envelope_response(
            &graph,
            Ok(Envelope::success(
                graph.identity(),
                None,
                serde_json::to_value(checkpoint).unwrap_or(Value::Null),
            )),
        ),
        Err(err) => envelope_response(&graph, Err(err)),
    }
}

async fn mcp(State(graph): State<EhGraph>, headers: HeaderMap, body: String) -> Response {
    let request: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => {
            return Json(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32700, "message": "parse error"},
            }))
            .into_response()
        }
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    if request.get("jsonrpc").and_then(|value| value.as_str()) != Some("2.0") {
        return Json(rpc_error(id, -32600, "invalid request")).into_response();
    }
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {
                "name": graph.identity().instance_id,
                "version": graph.build(),
            },
        }),
        "ping" | "notifications/initialized" => json!({}),
        "tools/list" => json!({"tools": tool_json(&graph)}),
        "tools/call" => {
            let header = auth_header(&headers);
            let name = params
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_owned();
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            let g = graph.clone();
            let envelope = or_failure(
                &graph,
                blocking(move || g.call_with_header(header.as_deref(), &name, &arguments)).await,
            );
            return Json(rpc_result(id, tool_result(&envelope))).into_response();
        }
        _ => return Json(rpc_error(id, -32601, "method not found")).into_response(),
    };
    Json(rpc_result(id, result)).into_response()
}

/// Run `f` on the blocking pool. A panic in the engine becomes an `UNAVAILABLE`
/// error rather than a dropped connection.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, EhError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| EhError::unavailable(format!("engine task failed: {err}")))
}

fn or_failure(graph: &EhGraph, result: Result<Envelope, EhError>) -> Envelope {
    result.unwrap_or_else(|err| Envelope::failure(graph.identity(), &err))
}

fn tool_json(graph: &EhGraph) -> Vec<Value> {
    let identity = graph.identity();
    ehgraph_api::tool_specs()
        .into_iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "description": format!("{} Instance {} domain {}.", spec.description, identity.instance_id, identity.domain),
                "inputSchema": spec.input_schema,
            })
        })
        .collect()
}

fn tool_result(envelope: &Envelope) -> Value {
    let text = serde_json::to_string(envelope).unwrap_or_else(|_| "{}".into());
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": !envelope.ok,
    })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn envelope_response(graph: &EhGraph, result: Result<Envelope, EhError>) -> Response {
    let envelope = match result {
        Ok(envelope) => envelope,
        Err(err) => Envelope::failure(graph.identity(), &err),
    };
    let status = if envelope.ok {
        StatusCode::OK
    } else {
        StatusCode::from_u16(
            envelope
                .error
                .as_ref()
                .map(|err| err.code.http_status())
                .unwrap_or(400),
        )
        .unwrap_or(StatusCode::BAD_REQUEST)
    };
    (status, Json(envelope)).into_response()
}
