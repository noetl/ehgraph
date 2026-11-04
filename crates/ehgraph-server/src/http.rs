use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ehgraph_api::Engine;
use ehgraph_core::{now_ms, EhError, Envelope};
use serde_json::{json, Value};

use crate::auth::authenticate;
use crate::config::Config;
use crate::metrics;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    pub config: Config,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics_route))
        .route("/mcp", post(mcp))
        .route("/mcp/tools", get(tools_list))
        .route("/v1/tools/{tool}", post(http_tool))
        .route("/v1/admin/backup", post(admin_backup))
        .with_state(Arc::new(state))
}

async fn healthz(State(app): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "instance_id": app.engine.identity.instance_id,
        "domain": app.engine.identity.domain,
        "role": app.engine.identity.role.as_str(),
        "state": "serving",
    }))
}

async fn metrics_route(State(app): State<Arc<AppState>>) -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        metrics::render(&app.engine),
    )
        .into_response()
}

async fn tools_list(State(app): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({"tools": tool_json(&app)}))
}

async fn http_tool(
    State(app): State<Arc<AppState>>,
    Path(tool): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let caller = match authenticate(&app.config.auth, &headers) {
        Ok(caller) => caller,
        Err(err) => return envelope_response(&app, Err(err)),
    };
    let args = if body.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        match serde_json::from_str::<Value>(&body) {
            Ok(value) => value,
            Err(err) => {
                return envelope_response(
                    &app,
                    Err(EhError::invalid(format!("request json: {err}"))),
                )
            }
        }
    };
    let envelope = app.engine.call(&tool, &args, &caller, now_ms());
    envelope_response(&app, Ok(envelope))
}

async fn admin_backup(State(app): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let caller = match authenticate(&app.config.auth, &headers) {
        Ok(caller) => caller,
        Err(err) => return envelope_response(&app, Err(err)),
    };
    if !caller.write_all {
        return envelope_response(
            &app,
            Err(EhError::unauthorized(
                "backup requires an operator grant (namespace \"*\")",
            )),
        );
    }
    match ehgraph_backup::ship(&app.engine.store, &app.engine.backup_prefix, now_ms()) {
        Ok(checkpoint) => envelope_response(
            &app,
            Ok(Envelope::success(
                &app.engine.identity,
                None,
                serde_json::to_value(checkpoint).unwrap_or(Value::Null),
            )),
        ),
        Err(err) => envelope_response(&app, Err(err)),
    }
}

async fn mcp(State(app): State<Arc<AppState>>, headers: HeaderMap, body: String) -> Response {
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
                "name": app.engine.identity.instance_id,
                "version": app.engine.build,
            },
        }),
        "ping" | "notifications/initialized" => json!({}),
        "tools/list" => json!({"tools": tool_json(&app)}),
        "tools/call" => {
            let caller = match authenticate(&app.config.auth, &headers) {
                Ok(caller) => caller,
                Err(err) => {
                    let envelope = Envelope::failure(&app.engine.identity, &err);
                    return Json(rpc_result(id, tool_result(&envelope))).into_response();
                }
            };
            let name = params
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            let envelope = app.engine.call(name, &arguments, &caller, now_ms());
            return Json(rpc_result(id, tool_result(&envelope))).into_response();
        }
        _ => return Json(rpc_error(id, -32601, "method not found")).into_response(),
    };
    Json(rpc_result(id, result)).into_response()
}

fn tool_json(app: &AppState) -> Vec<Value> {
    ehgraph_api::tool_specs()
        .into_iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "description": format!("{} Instance {} domain {}.", spec.description, app.engine.identity.instance_id, app.engine.identity.domain),
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

fn envelope_response(app: &AppState, result: Result<Envelope, EhError>) -> Response {
    let envelope = match result {
        Ok(envelope) => envelope,
        Err(err) => Envelope::failure(&app.engine.identity, &err),
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
