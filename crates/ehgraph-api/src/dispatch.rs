use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ehgraph_backup::{self, NamespaceCheckpoint};
use ehgraph_core::{
    now_ms, EhError, Envelope, ErrorCode, InstanceIdentity, Role, Scope, WriteRequest,
};
use ehgraph_memory::{
    Direction, NeighborQuery, NodeQuery, OpenRequest, PathQuery, SessionGraph, SessionRegistry,
    SubgraphQuery,
};
use ehgraph_store::{DeactivateRequest, Store};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub struct Caller {
    pub principal: String,
    pub read_all: bool,
    pub write_all: bool,
    pub read: BTreeSet<String>,
    pub write: BTreeSet<String>,
}

impl Caller {
    pub fn full(principal: impl Into<String>) -> Self {
        Self {
            principal: principal.into(),
            read_all: true,
            write_all: true,
            read: BTreeSet::new(),
            write: BTreeSet::new(),
        }
    }

    pub fn can_read(&self, namespace: &str) -> bool {
        self.read_all || self.read.contains(namespace)
    }

    pub fn can_write(&self, namespace: &str) -> bool {
        self.write_all || self.write.contains(namespace)
    }
}

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

pub struct Engine {
    pub store: Arc<Store>,
    pub sessions: Arc<SessionRegistry>,
    pub identity: InstanceIdentity,
    pub backup_prefix: PathBuf,
    pub build: String,
    pub global_bytes: u64,
    pub per_session_bytes: u64,
}

impl Engine {
    pub fn call(&self, tool: &str, args: &Value, caller: &Caller, now_ms: i64) -> Envelope {
        match self.dispatch(tool, args, caller, now_ms) {
            Ok(envelope) => envelope,
            Err(err) => Envelope::failure(&self.identity, &err),
        }
    }

    fn dispatch(
        &self,
        tool: &str,
        args: &Value,
        caller: &Caller,
        now_ms: i64,
    ) -> Result<Envelope, EhError> {
        if args
            .get("graph_session_id")
            .and_then(|v| v.as_str())
            .is_some()
            && matches!(
                tool,
                "graph.upsert" | "graph.deactivate" | "graph.snapshot.create"
            )
        {
            return Err(EhError::new(
                ErrorCode::ReadOnlySession,
                "session graphs are read-only; writes go to the change log without a session id",
            ));
        }
        match tool {
            "graph.upsert" => self.upsert(args, caller),
            "graph.deactivate" => self.deactivate(args, caller),
            "graph.snapshot.create" => self.snapshot_create(args, caller),
            "graph.node" => self.read_node(args, caller, now_ms),
            "graph.edge" => self.read_edge(args, caller, now_ms),
            "graph.query" => self.read_query(args, caller, now_ms),
            "graph.neighbors" => self.read_neighbors(args, caller, now_ms),
            "graph.subgraph" => self.read_subgraph(args, caller, now_ms),
            "graph.path" => self.read_path(args, caller, now_ms),
            "graph.snapshot.info" => self.snapshot_info(args, caller),
            "graph.session.open" => self.session_open(args, caller, now_ms),
            "graph.session.refresh" => self.session_refresh(args, caller, now_ms),
            "graph.session.close" => self.session_close(args, caller, now_ms),
            "graph.session.info" => self.session_info(args, caller, now_ms),
            "graph.session.list" => self.session_list(args, caller, now_ms),
            "graph.instance.info" => self.instance_info(),
            "graph.backup.status" => self.backup_status(args, caller),
            _ => Err(EhError::invalid(format!("unknown tool {tool}"))),
        }
    }

    fn upsert(&self, args: &Value, caller: &Caller) -> Result<Envelope, EhError> {
        let mut request: WriteRequest = serde_json::from_value(args.clone())
            .map_err(|err| EhError::invalid(format!("graph.upsert: {err}")))?;
        if request.ts_ms == 0 {
            request.ts_ms = now_ms();
        }
        self.require(caller, &request.namespace, true)?;
        let outcome = self.store.upsert(request)?;
        Ok(self.done(
            outcome.graph_version,
            json!({
                "graph_version": outcome.graph_version,
                "changed": outcome.changed,
                "applied": outcome.applied,
            }),
        ))
    }

    fn deactivate(&self, args: &Value, caller: &Caller) -> Result<Envelope, EhError> {
        #[derive(Deserialize)]
        struct Args {
            namespace: String,
            #[serde(default)]
            ids: Vec<String>,
            #[serde(default)]
            origin: Option<String>,
            #[serde(default)]
            source: Option<String>,
            #[serde(default)]
            not_seen_since: Option<i64>,
            #[serde(default)]
            idempotency_key: Option<String>,
            execution_id: String,
            #[serde(default)]
            ts_ms: i64,
        }
        let parsed: Args = serde_json::from_value(args.clone())
            .map_err(|err| EhError::invalid(format!("graph.deactivate: {err}")))?;
        self.require(caller, &parsed.namespace, true)?;
        let outcome = self.store.deactivate(DeactivateRequest {
            namespace: parsed.namespace,
            ids: parsed.ids,
            origin: parsed.origin,
            source: parsed.source,
            not_seen_since_ms: parsed.not_seen_since,
            idempotency_key: parsed.idempotency_key,
            execution_id: parsed.execution_id,
            ts_ms: if parsed.ts_ms == 0 {
                now_ms()
            } else {
                parsed.ts_ms
            },
        })?;
        Ok(self.done(
            outcome.graph_version,
            json!({
                "graph_version": outcome.graph_version,
                "changed": outcome.changed,
                "applied": outcome.applied,
            }),
        ))
    }

    fn snapshot_create(&self, args: &Value, caller: &Caller) -> Result<Envelope, EhError> {
        #[derive(Deserialize)]
        struct Args {
            namespace: String,
            #[serde(default)]
            scope: Option<Scope>,
        }
        let parsed: Args = serde_json::from_value(args.clone())
            .map_err(|err| EhError::invalid(format!("graph.snapshot.create: {err}")))?;
        self.require(caller, &parsed.namespace, true)?;
        let info = self
            .store
            .create_snapshot(&parsed.namespace, &parsed.scope.unwrap_or(Scope::Full))?;
        let version = info.graph_version;
        Ok(self.done(version, serde_json::to_value(info)?))
    }

    fn read_node(&self, args: &Value, caller: &Caller, now_ms: i64) -> Result<Envelope, EhError> {
        let located = locate(args)?;
        self.require(caller, &located.namespace, false)?;
        let (version, node) = self.with_graph(&located, now_ms, |graph| {
            graph
                .node(&located.id.clone().unwrap_or_default())
                .cloned()
                .ok_or_else(|| {
                    EhError::not_found(format!(
                        "node {} not found",
                        located.id.as_deref().unwrap_or("")
                    ))
                })
        })?;
        Ok(self.done(version, serde_json::to_value(node)?))
    }

    fn read_edge(&self, args: &Value, caller: &Caller, now_ms: i64) -> Result<Envelope, EhError> {
        let located = locate(args)?;
        self.require(caller, &located.namespace, false)?;
        let id = located
            .id
            .clone()
            .ok_or_else(|| EhError::invalid("id is required"))?;
        let (version, edge) = self.with_graph(&located, now_ms, |graph| {
            graph
                .edge(&id)
                .cloned()
                .ok_or_else(|| EhError::not_found(format!("edge {id} not found")))
        })?;
        Ok(self.done(version, serde_json::to_value(edge)?))
    }

    fn read_query(&self, args: &Value, caller: &Caller, now_ms: i64) -> Result<Envelope, EhError> {
        #[derive(Deserialize)]
        struct Args {
            namespace: String,
            #[serde(default)]
            graph_session_id: Option<String>,
            #[serde(default)]
            version: Option<u64>,
            #[serde(rename = "type", default)]
            type_name: Option<String>,
            #[serde(default)]
            origin: Option<String>,
            #[serde(default)]
            props: std::collections::BTreeMap<String, ehgraph_core::PropPredicate>,
            #[serde(default)]
            active: ehgraph_core::ActiveFilter,
            #[serde(default = "default_limit")]
            limit: usize,
            #[serde(default)]
            cursor: Option<String>,
        }
        let parsed: Args = serde_json::from_value(args.clone())
            .map_err(|err| EhError::invalid(format!("graph.query: {err}")))?;
        self.require(caller, &parsed.namespace, false)?;
        let located = Located {
            namespace: parsed.namespace,
            graph_session_id: parsed.graph_session_id,
            version: parsed.version,
            id: None,
        };
        let query = NodeQuery {
            type_name: parsed.type_name,
            origin: parsed.origin,
            props: parsed.props,
            active: parsed.active,
            limit: parsed.limit,
            cursor: parsed.cursor,
        };
        let (version, page) = self.with_graph(&located, now_ms, |graph| Ok(graph.query(&query)))?;
        Ok(self.page(
            version,
            json!({"nodes": page.items}),
            page.truncated,
            page.next_cursor,
        ))
    }

    fn read_neighbors(
        &self,
        args: &Value,
        caller: &Caller,
        now_ms: i64,
    ) -> Result<Envelope, EhError> {
        #[derive(Deserialize)]
        struct Args {
            namespace: String,
            node_id: String,
            #[serde(default)]
            graph_session_id: Option<String>,
            #[serde(default)]
            version: Option<u64>,
            #[serde(default)]
            edge_type: Option<String>,
            #[serde(default = "default_direction")]
            direction: String,
            #[serde(default = "default_limit")]
            limit: usize,
            #[serde(default)]
            cursor: Option<String>,
        }
        let parsed: Args = serde_json::from_value(args.clone())
            .map_err(|err| EhError::invalid(format!("graph.neighbors: {err}")))?;
        self.require(caller, &parsed.namespace, false)?;
        let direction = parse_direction(&parsed.direction)?;
        let located = Located {
            namespace: parsed.namespace,
            graph_session_id: parsed.graph_session_id,
            version: parsed.version,
            id: None,
        };
        let query = NeighborQuery {
            node_id: parsed.node_id,
            edge_type: parsed.edge_type,
            direction,
            limit: parsed.limit,
            cursor: parsed.cursor,
            active_only: true,
        };
        let (version, page) = self.with_graph(&located, now_ms, |graph| graph.neighbors(&query))?;
        Ok(self.page(
            version,
            json!({"edges": page.items}),
            page.truncated,
            page.next_cursor,
        ))
    }

    fn read_subgraph(
        &self,
        args: &Value,
        caller: &Caller,
        now_ms: i64,
    ) -> Result<Envelope, EhError> {
        #[derive(Deserialize)]
        struct Args {
            namespace: String,
            seeds: Vec<String>,
            k: u32,
            #[serde(default)]
            graph_session_id: Option<String>,
            #[serde(default)]
            version: Option<u64>,
            #[serde(default)]
            edge_types: Option<Vec<String>>,
            #[serde(default = "default_max_nodes")]
            max_nodes: usize,
        }
        let parsed: Args = serde_json::from_value(args.clone())
            .map_err(|err| EhError::invalid(format!("graph.subgraph: {err}")))?;
        self.require(caller, &parsed.namespace, false)?;
        let located = Located {
            namespace: parsed.namespace,
            graph_session_id: parsed.graph_session_id,
            version: parsed.version,
            id: None,
        };
        let query = SubgraphQuery {
            seeds: parsed.seeds,
            k: parsed.k,
            edge_types: parsed.edge_types,
            max_nodes: parsed.max_nodes,
        };
        let (version, sub) = self.with_graph(&located, now_ms, |graph| graph.subgraph(&query))?;
        let truncated = sub.truncated;
        Ok(self.page(version, serde_json::to_value(sub)?, truncated, None))
    }

    fn read_path(&self, args: &Value, caller: &Caller, now_ms: i64) -> Result<Envelope, EhError> {
        #[derive(Deserialize)]
        struct Args {
            namespace: String,
            src: String,
            dst: String,
            #[serde(default)]
            graph_session_id: Option<String>,
            #[serde(default)]
            version: Option<u64>,
            #[serde(default)]
            edge_types: Option<Vec<String>>,
            #[serde(default = "default_hops")]
            max_hops: u32,
        }
        let parsed: Args = serde_json::from_value(args.clone())
            .map_err(|err| EhError::invalid(format!("graph.path: {err}")))?;
        self.require(caller, &parsed.namespace, false)?;
        let located = Located {
            namespace: parsed.namespace,
            graph_session_id: parsed.graph_session_id,
            version: parsed.version,
            id: None,
        };
        let query = PathQuery {
            src: parsed.src,
            dst: parsed.dst,
            edge_types: parsed.edge_types,
            max_hops: parsed.max_hops,
        };
        let (version, path) = self.with_graph(&located, now_ms, |graph| graph.path(&query))?;
        Ok(self.done(version, serde_json::to_value(path)?))
    }

    fn snapshot_info(&self, args: &Value, caller: &Caller) -> Result<Envelope, EhError> {
        #[derive(Deserialize)]
        struct Args {
            namespace: String,
            #[serde(default)]
            version: Option<u64>,
        }
        let parsed: Args = serde_json::from_value(args.clone())
            .map_err(|err| EhError::invalid(format!("graph.snapshot.info: {err}")))?;
        self.require(caller, &parsed.namespace, false)?;
        let info = self.store.version_info(&parsed.namespace, parsed.version)?;
        let version = info.graph_version;
        Ok(self.done(version, serde_json::to_value(info)?))
    }

    fn session_open(
        &self,
        args: &Value,
        caller: &Caller,
        now_ms: i64,
    ) -> Result<Envelope, EhError> {
        #[derive(Deserialize)]
        struct Args {
            namespace: String,
            owner: String,
            #[serde(default)]
            scope: Option<Scope>,
            #[serde(default)]
            version: Option<u64>,
            #[serde(default)]
            idle_timeout_s: Option<u64>,
        }
        let parsed: Args = serde_json::from_value(args.clone())
            .map_err(|err| EhError::invalid(format!("graph.session.open: {err}")))?;
        self.require(caller, &parsed.namespace, false)?;
        let scope = parsed.scope.unwrap_or(Scope::Full);
        let namespace = parsed.namespace.clone();
        let version = parsed.version;
        let store = self.store.clone();
        let outcome = self.sessions.open(
            OpenRequest {
                owner: parsed.owner,
                namespace: parsed.namespace,
                scope: scope.clone(),
                requested_version: version,
                idle_timeout: parsed.idle_timeout_s.map(Duration::from_secs),
                now_ms,
            },
            move || load_scope(&store, &namespace, &scope, version),
        )?;
        let version = outcome.snapshot_version;
        Ok(self.done(version, serde_json::to_value(outcome)?))
    }

    fn session_refresh(
        &self,
        args: &Value,
        caller: &Caller,
        now_ms: i64,
    ) -> Result<Envelope, EhError> {
        #[derive(Deserialize)]
        struct Args {
            graph_session_id: String,
            #[serde(default)]
            to_version: Option<u64>,
        }
        let parsed: Args = serde_json::from_value(args.clone())
            .map_err(|err| EhError::invalid(format!("graph.session.refresh: {err}")))?;
        let info = self.sessions.info(&parsed.graph_session_id, now_ms)?;
        self.require(caller, &info.namespace, false)?;
        let store = self.store.clone();
        let namespace = info.namespace.clone();
        let scope: Scope = serde_json::from_str(&info.scope).unwrap_or(Scope::Full);
        let outcome = self
            .sessions
            .refresh(&parsed.graph_session_id, now_ms, move |_current| {
                let version = parsed.to_version;
                load_scope(&store, &namespace, &scope, version)
            })?;
        let version = outcome.snapshot_version;
        Ok(self.done(version, serde_json::to_value(outcome)?))
    }

    fn session_close(
        &self,
        args: &Value,
        caller: &Caller,
        now_ms: i64,
    ) -> Result<Envelope, EhError> {
        let id = required_session(args)?;
        let info = self.sessions.info(&id, now_ms)?;
        self.require(caller, &info.namespace, false)?;
        let outcome = self.sessions.close(&id, now_ms)?;
        Ok(self.done(info.snapshot_version, serde_json::to_value(outcome)?))
    }

    fn session_info(
        &self,
        args: &Value,
        caller: &Caller,
        now_ms: i64,
    ) -> Result<Envelope, EhError> {
        let id = required_session(args)?;
        let info = self.sessions.info(&id, now_ms)?;
        self.require(caller, &info.namespace, false)?;
        Ok(self.done(info.snapshot_version, serde_json::to_value(info)?))
    }

    fn session_list(
        &self,
        args: &Value,
        caller: &Caller,
        now_ms: i64,
    ) -> Result<Envelope, EhError> {
        #[derive(Deserialize, Default)]
        struct Args {
            #[serde(default)]
            namespace: Option<String>,
            #[serde(default)]
            owner: Option<String>,
        }
        let parsed: Args = if args.is_null() {
            Args::default()
        } else {
            serde_json::from_value(args.clone())
                .map_err(|err| EhError::invalid(format!("graph.session.list: {err}")))?
        };
        if let Some(namespace) = &parsed.namespace {
            self.require(caller, namespace, false)?;
        }
        let mut sessions =
            self.sessions
                .list(parsed.namespace.as_deref(), parsed.owner.as_deref(), now_ms)?;
        if !caller.read_all {
            sessions.retain(|session| caller.can_read(&session.namespace));
        }
        Ok(Envelope::success(
            &self.identity,
            None,
            json!({"sessions": sessions}),
        ))
    }

    fn instance_info(&self) -> Result<Envelope, EhError> {
        let namespaces: Vec<_> = self
            .store
            .summaries()?
            .into_iter()
            .map(|summary| {
                json!({
                    "namespace": summary.namespace,
                    "head_version": summary.head_version,
                    "sealed_version": summary.sealed_version,
                    "nodes": summary.nodes,
                    "edges": summary.edges,
                })
            })
            .collect();
        Ok(Envelope::success(
            &self.identity,
            None,
            json!({
                "instance_id": self.identity.instance_id,
                "domain": self.identity.domain,
                "role": self.identity.role.as_str(),
                "build": self.build,
                "budgets": {
                    "global_bytes": self.global_bytes,
                    "per_session_bytes": self.per_session_bytes,
                },
                "namespaces": namespaces,
            }),
        ))
    }

    fn backup_status(&self, args: &Value, caller: &Caller) -> Result<Envelope, EhError> {
        #[derive(Deserialize, Default)]
        struct Args {
            #[serde(default)]
            namespace: Option<String>,
        }
        let parsed: Args = if args.is_null() {
            Args::default()
        } else {
            serde_json::from_value(args.clone())
                .map_err(|err| EhError::invalid(format!("graph.backup.status: {err}")))?
        };
        if let Some(namespace) = &parsed.namespace {
            self.require(caller, namespace, false)?;
        }
        let checkpoint = ehgraph_backup::read_checkpoint(&self.backup_prefix)?;
        let now = now_ms();
        let mut rows = Vec::new();
        for summary in self.store.summaries()? {
            if parsed
                .namespace
                .as_ref()
                .is_some_and(|name| name != &summary.namespace)
            {
                continue;
            }
            if !caller.can_read(&summary.namespace) {
                continue;
            }
            let saved: Option<&NamespaceCheckpoint> = checkpoint
                .as_ref()
                .and_then(|item| item.namespaces.get(&summary.namespace));
            let (lag_versions, lag_seconds) = ehgraph_backup::lag(summary.head_version, saved, now);
            rows.push(json!({
                "namespace": summary.namespace,
                "head_version": summary.head_version,
                "backed_up_version": saved.map(|item| item.backed_up_version).unwrap_or(0),
                "lag_versions": lag_versions,
                "lag_seconds": lag_seconds,
                "shipped_at_ms": saved.map(|item| item.shipped_at_ms),
            }));
        }
        Ok(Envelope::success(
            &self.identity,
            None,
            json!({"namespaces": rows}),
        ))
    }

    fn with_graph<T>(
        &self,
        located: &Located,
        now_ms: i64,
        f: impl FnOnce(&SessionGraph) -> Result<T, EhError>,
    ) -> Result<(u64, T), EhError> {
        if let Some(session_id) = &located.graph_session_id {
            let mut slot = None;
            self.sessions.read(session_id, now_ms, |graph| {
                slot = Some(match f(graph) {
                    Ok(value) => Ok((graph.version, value)),
                    Err(err) => Err(err),
                });
            })?;
            return slot
                .unwrap_or_else(|| Err(EhError::unavailable("session read produced no result")));
        }
        let materialized = self.store.graph_at(&located.namespace, located.version)?;
        let graph = SessionGraph::from_materialized(&materialized)?;
        let version = graph.version;
        Ok((version, f(&graph)?))
    }

    fn require(&self, caller: &Caller, namespace: &str, write: bool) -> Result<(), EhError> {
        let allowed = if write {
            caller.can_write(namespace)
        } else {
            caller.can_read(namespace)
        };
        if allowed {
            Ok(())
        } else {
            Err(EhError::unauthorized(format!(
                "principal {} lacks {} access to namespace {namespace}",
                caller.principal,
                if write { "write" } else { "read" }
            )))
        }
    }

    fn done(&self, version: u64, data: Value) -> Envelope {
        Envelope::success(&self.identity, Some(version), data)
    }

    fn page(&self, version: u64, data: Value, truncated: bool, cursor: Option<String>) -> Envelope {
        self.done(version, data).with_page(truncated, cursor)
    }
}

struct Located {
    namespace: String,
    graph_session_id: Option<String>,
    version: Option<u64>,
    id: Option<String>,
}

fn locate(args: &Value) -> Result<Located, EhError> {
    #[derive(Deserialize)]
    struct Args {
        namespace: String,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        graph_session_id: Option<String>,
        #[serde(default)]
        version: Option<u64>,
    }
    let parsed: Args =
        serde_json::from_value(args.clone()).map_err(|err| EhError::invalid(err.to_string()))?;
    if parsed.id.as_deref().is_none() {
        return Err(EhError::invalid("id is required"));
    }
    Ok(Located {
        namespace: parsed.namespace,
        graph_session_id: parsed.graph_session_id,
        version: parsed.version,
        id: parsed.id,
    })
}

fn required_session(args: &Value) -> Result<String, EhError> {
    args.get("graph_session_id")
        .and_then(|value| value.as_str())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| EhError::invalid("graph_session_id is required"))
}

fn load_scope(
    store: &Store,
    namespace: &str,
    scope: &Scope,
    version: Option<u64>,
) -> Result<SessionGraph, EhError> {
    let graph = if let Scope::Snapshot { snapshot_id } = scope {
        store.load_snapshot(namespace, snapshot_id)?
    } else {
        store.graph_at(namespace, version)?.apply_scope(scope)?
    };
    SessionGraph::from_materialized(&graph)
}

fn parse_direction(value: &str) -> Result<Direction, EhError> {
    match value {
        "out" => Ok(Direction::Out),
        "in" => Ok(Direction::In),
        "both" => Ok(Direction::Both),
        other => Err(EhError::invalid(format!(
            "direction must be out, in, or both, got {other}"
        ))),
    }
}

fn default_limit() -> usize {
    100
}
fn default_max_nodes() -> usize {
    100
}
fn default_hops() -> u32 {
    16
}
fn default_direction() -> String {
    "out".into()
}

pub fn tool_specs() -> Vec<ToolSpec> {
    let object = |props: &[(&str, &str, bool)]| {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for (name, type_name, is_required) in props {
            properties.insert((*name).to_string(), json!({"type": type_name}));
            if *is_required {
                required.push(*name);
            }
        }
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
        })
    };
    vec![
        spec(
            "graph.upsert",
            "Batch upsert nodes and edges.",
            object(&[
                ("namespace", "string", true),
                ("nodes", "array", false),
                ("edges", "array", false),
                ("idempotency_key", "string", false),
                ("source", "string", true),
                ("execution_id", "string", true),
            ]),
        ),
        spec(
            "graph.deactivate",
            "Soft-deactivate elements by id or by source.",
            object(&[
                ("namespace", "string", true),
                ("ids", "array", false),
                ("source", "string", false),
                ("not_seen_since", "integer", false),
                ("execution_id", "string", true),
            ]),
        ),
        spec(
            "graph.snapshot.create",
            "Write an immutable snapshot.",
            object(&[("namespace", "string", true), ("scope", "object", false)]),
        ),
        spec(
            "graph.node",
            "Fetch one node.",
            object(&[
                ("namespace", "string", true),
                ("id", "string", true),
                ("graph_session_id", "string", false),
                ("version", "integer", false),
            ]),
        ),
        spec(
            "graph.edge",
            "Fetch one edge.",
            object(&[
                ("namespace", "string", true),
                ("id", "string", true),
                ("graph_session_id", "string", false),
            ]),
        ),
        spec(
            "graph.query",
            "Filter nodes.",
            object(&[
                ("namespace", "string", true),
                ("type", "string", false),
                ("limit", "integer", false),
                ("graph_session_id", "string", false),
            ]),
        ),
        spec(
            "graph.neighbors",
            "Edges adjacent to a node.",
            object(&[
                ("namespace", "string", true),
                ("node_id", "string", true),
                ("direction", "string", false),
                ("graph_session_id", "string", false),
            ]),
        ),
        spec(
            "graph.subgraph",
            "Directed k-hop subgraph.",
            object(&[
                ("namespace", "string", true),
                ("seeds", "array", true),
                ("k", "integer", true),
            ]),
        ),
        spec(
            "graph.path",
            "Directed shortest path.",
            object(&[
                ("namespace", "string", true),
                ("src", "string", true),
                ("dst", "string", true),
            ]),
        ),
        spec(
            "graph.snapshot.info",
            "Counts at a version.",
            object(&[("namespace", "string", true), ("version", "integer", false)]),
        ),
        spec(
            "graph.session.open",
            "Load a scope into session memory.",
            object(&[
                ("namespace", "string", true),
                ("owner", "string", true),
                ("scope", "object", false),
            ]),
        ),
        spec(
            "graph.session.refresh",
            "Atomically swap the pinned version.",
            object(&[("graph_session_id", "string", true)]),
        ),
        spec(
            "graph.session.close",
            "End a session.",
            object(&[("graph_session_id", "string", true)]),
        ),
        spec(
            "graph.session.info",
            "Describe one session.",
            object(&[("graph_session_id", "string", true)]),
        ),
        spec(
            "graph.session.list",
            "List sessions.",
            object(&[("namespace", "string", false)]),
        ),
        spec(
            "graph.instance.info",
            "Identity, role, namespaces, budgets.",
            object(&[]),
        ),
        spec(
            "graph.backup.status",
            "Backup lag per namespace.",
            object(&[("namespace", "string", false)]),
        ),
    ]
}

fn spec(name: &'static str, description: &'static str, input_schema: Value) -> ToolSpec {
    ToolSpec {
        name,
        description,
        input_schema,
    }
}

/// Role gate used by the server before it accepts writes. Replicas are refused at startup.
pub fn ensure_primary(role: Role) -> Result<(), EhError> {
    match role {
        Role::Primary => Ok(()),
        Role::Replica | Role::Standby => Err(EhError::new(
            ErrorCode::Unavailable,
            "read replicas and standby are not enabled; Stage 1 needs election and store-side fencing",
        )),
    }
}
