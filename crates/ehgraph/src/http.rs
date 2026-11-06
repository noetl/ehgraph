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
