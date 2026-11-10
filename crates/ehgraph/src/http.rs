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
use axum::routing::{get, post};
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
    let request: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(err) => {
            return neo4j_error(
                StatusCode::BAD_REQUEST,
                "Neo.ClientError.Request.Invalid",
                &format!("request json: {err}"),
            )
        }
    };
    let Some(statement) = request.get("statement").and_then(Value::as_str) else {
        return neo4j_error(
            StatusCode::BAD_REQUEST,
            "Neo.ClientError.Request.Invalid",
            "`statement` is required",
        );
    };
    let args = json!({
        "namespace": namespace,
        "query": statement,
        "params": request.get("parameters").cloned().unwrap_or(json!({})),
    });
    let header = auth_header(&headers);
    let g = graph.clone();
    let envelope = or_failure(
        &graph,
        blocking(move || g.call_with_header(header.as_deref(), "graph.cypher", &args)).await,
    );
    if !envelope.ok {
        let err = envelope.error.as_ref();
        let message = err.map(|e| e.message.clone()).unwrap_or_default();
        let code = match ehgraph_cypher::kind_of_message(&message) {
            Some(kind) => kind.neo4j_code(),
            None => match err.map(|e| e.code) {
                Some(ehgraph_core::ErrorCode::Unauthorized) => {
                    "Neo.ClientError.Security.Unauthorized"
                }
                Some(ehgraph_core::ErrorCode::NotFound) => {
                    "Neo.ClientError.Database.DatabaseNotFound"
                }
                _ => "Neo.ClientError.Statement.ArgumentError",
            },
        };
        let status = StatusCode::from_u16(err.map(|e| e.code.http_status()).unwrap_or(400))
            .unwrap_or(StatusCode::BAD_REQUEST);
        return neo4j_error(status, code, &message);
    }
    let data = envelope.data.unwrap_or(Value::Null);
    let stats = &data["stats"];
    let n = |key: &str| stats.get(key).and_then(Value::as_u64).unwrap_or(0);
    (
        StatusCode::ACCEPTED,
        Json(json!({
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
            "bookmarks": [format!("ehgraph:{}:{}", namespace, envelope.graph_version.unwrap_or(0))],
        })),
    )
        .into_response()
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
