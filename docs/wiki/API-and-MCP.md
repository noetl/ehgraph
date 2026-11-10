# API and MCP

Every instance exposes the same tools. You choose the domain by which server you call.

- MCP: `POST /mcp` with JSON-RPC 2.0. Methods: `initialize`, `ping`, `tools/list`, `tools/call`.
- HTTP: `POST /v1/tools/{tool}` with the tool arguments as a JSON object. `{tool}` is one path segment, so `graph.upsert` is valid.
- Discovery: `GET /mcp/tools`.
- Cypher: `POST /v1/cypher`, `POST /db/{namespace}/query/v2` (Neo4j Query API shape), and an optional Bolt listener. See [Cypher](Cypher).
- Health: `GET /healthz`.
- Metrics: `GET /metrics` (Prometheus text).
- Operator ship: `POST /v1/admin/backup` (requires an operator grant, `namespace = "*"`).

`tools/call` arguments are the same JSON as the HTTP body. The MCP result is `{content:[{type:"text", text:<envelope json>}], isError}`.

## Envelope

```json
{
  "ok": true,
  "instance_id": "ehgraph-network",
  "domain": "network",
  "graph_version": 3,
  "data": {},
  "truncated": true,
  "next_cursor": "device:sw2"
}
```

`truncated` and `next_cursor` appear only when a page was cut off. Pass `cursor` back to continue. Errors use the same envelope with `ok: false` and:

```json
{"error": {"code": "NOT_FOUND", "message": "..."}}
```

| Code | When |
|---|---|
| `NOT_FOUND` | Unknown namespace, id, snapshot, or session id |
| `INVALID_ARGUMENT` | Bad ids, dangling edge, schema rejection, empty upsert |
| `UNAUTHORIZED` | Missing credential, or no grant for that namespace |
| `SESSION_EXPIRED` | Closed, expired, or evicted session |
| `SESSION_LOADING` | Reserved for a session that is not ready |
| `MEMORY_BUDGET_EXCEEDED` | Per-session cap or global budget |
| `READ_ONLY_SESSION` | Write tools given a `graph_session_id` |
| `CONFLICT` | Data dir belongs to another instance, or an idempotency key was reused with a different payload |
| `UNAVAILABLE` | Corrupt log, or a non-primary role |

HTTP status follows the code (`401`, `403`, `404`, `409`, `400`, `503`). MCP still returns HTTP 200 and sets `isError`.

## Tools

Read tools take `namespace` and either `graph_session_id` or `version`. With a session, the pinned version wins. Without one, the read uses the durable head, or the version you pass.

| Tool | Role |
|---|---|
| `graph.upsert` | Batch of `nodes` and `edges`. Returns the version. Same ids and same content do not bump it. |
| `graph.deactivate` | `ids`, or `{source, not_seen_since}` plus optional `origin`. Soft-deactivate. |
| `graph.snapshot.create` | Arrow IPC snapshot at the current head. `scope` defaults to full. |
| `graph.node` / `graph.edge` | One element by `id`. |
| `graph.cypher` | openCypher subset: read and write. `query`, `params`, optional `active`, `version`/`graph_session_id` (read only), `source`, `idempotency_key`. Writes commit through `graph.upsert` + `graph.deactivate`. See [Cypher](Cypher). |
| `graph.query` | Nodes by `type`, `origin`, `props`, `active` (`true`, `false`, or `"any"`). `props` values are equality, or `{gte, gt, lte, lt}`. |
| `graph.neighbors` | `node_id`, `direction` (`out`, `in`, `both`), optional `edge_type`. |
| `graph.subgraph` | Directed k-hop from `seeds`. Optional `edge_types` and `max_nodes`. |
| `graph.path` | Directed shortest path from `src` to `dst`. |
| `graph.snapshot.info` | Counts at a version. |
| `graph.session.open` | `owner`, `scope`, optional `version` and `idle_timeout_s`. |
| `graph.session.refresh` | Swap in the head, or `to_version`. |
| `graph.session.close` | Release. Later reads return `SESSION_EXPIRED`. |
| `graph.session.info` / `graph.session.list` | State, scope, version, bytes, expiry. |
| `graph.instance.info` | Identity, role, budgets, namespaces. |
| `graph.backup.status` | Per namespace: head, backed-up version, lag. |

`scope` is `{kind: full}`, `{kind: filter, type, props}`, `{kind: khop, seeds, k, edge_types}`, or `{kind: snapshot, snapshot_id}`.

## Properties and time

JSON numbers without a fraction are integers. Nested objects and arrays are allowed. A timestamp is `{"$timestamp":"<rfc3339>"}` or `{"$timestamp": <unix millis>}`. `first_seen`, `last_seen`, `ts_ms`, and `not_seen_since` on the write tools are unix milliseconds. `ts_ms` of `0` or omitted means the server clock. Idempotency hashes the namespace, elements, and `source`. Retries may use a new `execution_id` and `ts_ms`. The same key with a different payload is `CONFLICT`.

Keys are remembered for a bounded window per namespace:

- **Time:** `[store] idempotency_window_s`, 24 h by default. It is measured on the **log clock**, which is the highest `ts_ms` committed so far and never goes backwards. It is not the wall clock, so reopening a namespace rebuilds exactly the same window.
- **Count:** `[store] idempotency_max_keys`, 100,000 by default. The oldest keys go first.

A retry after its key is forgotten is a new write, planned against the current graph:

- an unchanged upsert is a no-op;
- a retry that lands after someone else changed the same elements re-applies the old payload;
- a reused key with a different payload is accepted, not reported as `CONFLICT`.

Set the window longer than any caller's retry horizon. `ehgraph_idempotency_keys`, `ehgraph_idempotency_key_bytes` and `ehgraph_idempotency_keys_evicted` report it per namespace.

An edge whose `src` or `dst` is missing or inactive is rejected. The whole batch is rejected. No placeholder nodes are created. Omit an edge `id` and the server uses `edge:<src>:<type>:<dst>:<origin>`.

## Catalog resource

Each instance is one `kind: Mcp` resource. See `playbooks/automation/agents/ehgraph/resources/`. The name is `ehgraph-<domain>` and the path is `mcp/ehgraph/<domain>`. Lifecycle playbooks are shared. The NoETL dispatcher passes the resource as `workload.mcp_resource`, which was confirmed against the kubernetes MCP lifecycle agent in `noetl/ops`.
