# Contract: ehgraph tool surface (MVP)

ehgraph is a topology database server. **Each instance** serves one topology domain and
is exposed as:

- MCP tools: JSON-RPC `tools/list` / `tools/call`, callable from playbooks with `kind: mcp`
- equivalent HTTP routes: `POST /v1/tools/{tool}`, with the same JSON args

Every instance has the same tool surface. Covers FR-005–FR-025.

## Catalog resources: one per instance (FR-022, FR-023)

Field names follow `docs/docs/architecture/mcp_catalog_architecture.md` (noetl ≥ 2.26),
checked 2026-11-03.

Each instance is registered as its own `kind: Mcp` resource. The naming convention, for
`<domain>` = `noetl-cluster` | `network` | …:

| Field | Value |
|---|---|
| `metadata.name` | `ehgraph-<domain>` |
| `metadata.path` | `mcp/ehgraph/<domain>` |
| Service DNS | `ehgraph-<domain>.ehgraph.svc.cluster.local` (one StatefulSet + Service per instance in the shared K8s namespace `ehgraph`) |
| Lifecycle playbooks | shared under `automation/agents/ehgraph/lifecycle/*`, parameterized by instance |

Example, network domain (qnetmon):

```yaml
apiVersion: noetl.io/v2
kind: Mcp
metadata:
  name: ehgraph-network
  path: mcp/ehgraph/network
spec:
  url: http://ehgraph-network.ehgraph.svc.cluster.local:8080/mcp   # [NEEDS CLARIFICATION FR-009 targets]
  protocol: mcp/1.0
  lifecycle:
    deploy:    automation/agents/ehgraph/lifecycle/deploy
    undeploy:  automation/agents/ehgraph/lifecycle/undeploy
    redeploy:  automation/agents/ehgraph/lifecycle/redeploy
    restart:   automation/agents/ehgraph/lifecycle/restart
    status:    automation/agents/ehgraph/lifecycle/status
    discover:  automation/agents/ehgraph/lifecycle/discover
  discovery:
    initialize_url: http://ehgraph-network.ehgraph.svc.cluster.local:8080/healthz
    tools_list_url: http://ehgraph-network.ehgraph.svc.cluster.local:8080/mcp/tools
    refresh_via:    automation/agents/ehgraph/lifecycle/discover
  deployment:
    namespace: ehgraph
```

`ehgraph-noetl-cluster` is identical, with `network` replaced by `noetl-cluster`.

Instance parameters:

- The shared lifecycle playbooks derive the instance from the resource: `domain` = the
  last segment of `metadata.path`, and `instance_id` = `metadata.name`.
- They render the instance-specific StatefulSet (`replicas: 1`, RWO volume), Service,
  config (`instance_id`, `domain`, `data_dir`, `backup_prefix`, budgets) and backup
  credentials.
- [NEEDS CLARIFICATION: does the `kind: Mcp` lifecycle dispatch pass the resource (or its
  path) to the playbook? If not, each instance gets thin wrapper playbooks under
  `automation/agents/ehgraph/<domain>/lifecycle/*` that call the shared ones with
  `instance`.]

Other rules:

- **Backup and restore** (FR-024) are separate playbooks, because `kind: Mcp` lifecycle
  has no such verbs: `automation/agents/ehgraph/ops/backup` and
  `automation/agents/ehgraph/ops/restore`, each with arg `instance`.
- **No cross-instance routing.** A consumer picks the domain by calling the matching
  resource. A call that names a namespace the instance does not hold returns `NOT_FOUND`.

Credentials come from the NoETL keychain. [NEEDS CLARIFICATION FR-008: token shape.]

## Result envelope

Every tool returns
`{ ok, instance_id, domain, graph_version, data, truncated?, next_cursor?, error? }`.

`error.code` is one of:

- `NOT_FOUND`
- `INVALID_ARGUMENT`
- `UNAUTHORIZED`
- `SESSION_EXPIRED`
- `SESSION_LOADING`
- `MEMORY_BUDGET_EXCEEDED` (FR-016)
- `READ_ONLY_SESSION` (FR-019)
- `CONFLICT`
- `UNAVAILABLE`

## Session tools (FR-010–FR-020)

| Tool | Args | Data |
|---|---|---|
| `graph.session.open` | `namespace`, `owner`, `scope`, `version?`, `idle_timeout_s?` | `graph_session_id`, `snapshot_version`, `resident_bytes`, `load_ms`, `reused` |
| `graph.session.refresh` | `graph_session_id`, `to_version?` | `snapshot_version`, `previous_version` (atomic swap) |
| `graph.session.close` | `graph_session_id` | `released_bytes` |
| `graph.session.info` | `graph_session_id` | state, scope, version, bytes, `last_read_at`, `expires_at` |
| `graph.session.list` | `namespace?`, `owner?` | sessions (admin/operability) |

`scope` is `{ kind: full }` \| `{ kind: filter, type?, props }` \|
`{ kind: khop, seeds[], k, edge_types? }` \| `{ kind: snapshot, snapshot_id }`.
[NEEDS CLARIFICATION FR-011]

## Read tools (FR-005)

All read tools take `namespace` and an optional `graph_session_id?`. When the session id
is absent, they read the durable store at head, or at `version?`.

| Tool | Args |
|---|---|
| `graph.node` / `graph.edge` | `id` |
| `graph.query` | `type?`, `origin?`, `props` (equality/range filters), `active?` (default true), `limit`, `cursor` |
| `graph.neighbors` | `node_id`, `edge_type?`, `direction` (out\|in\|both), `limit` |
| `graph.subgraph` | `seeds[]`, `k`, `edge_types?`, `max_nodes` |
| `graph.path` | `src`, `dst`, `edge_types?`, `max_hops` |
| `graph.snapshot.info` | `version?` → counts, bytes, `created_at` |

[NEEDS CLARIFICATION FR-005: openCypher/GQL subset beyond these operations.]

## Write tools (FR-007)

Write tools are rejected with `READ_ONLY_SESSION` when given a `graph_session_id`.

| Tool | Args | Data |
|---|---|---|
| `graph.upsert` | `namespace`, `nodes[]`, `edges[]`, `idempotency_key?`, `source`, `execution_id` | `graph_version`, counts |
| `graph.deactivate` | `namespace`, `ids[]` or `{origin?, source, not_seen_since}` | `graph_version`, count |
| `graph.snapshot.create` | `namespace`, `scope?` | `snapshot_id`, `graph_version` |

## Instance tools (FR-022, FR-024)

| Tool | Args | Data |
|---|---|---|
| `graph.instance.info` | none | `instance_id`, `domain`, `role` (`primary` in the MVP), namespaces with `head_version`, budgets, build |
| `graph.backup.status` | `namespace?` | per namespace: `head_version`, `backed_up_version`, lag (versions, seconds), last `shipped_at` |

Restore is an offline operation run by the `ops/restore` playbook (empty volume, then
start in `restoring` state). It is not an online tool.

## Operability (FR-009)

- `GET /healthz`: used by the `status` lifecycle verb. Reports `instance_id`, `domain`,
  `role` and `state` (starting | restoring | serving | draining).
- `GET /metrics`: every series is labelled `instance_id` and `domain`. The series:
  - `ehgraph_sessions{state}`
  - `ehgraph_resident_bytes`
  - `ehgraph_load_seconds`
  - `ehgraph_evictions_total{reason}`
  - `ehgraph_graph_version{namespace}`
  - `ehgraph_backup_lag_versions{namespace}`
  - `ehgraph_backup_lag_seconds{namespace}`
- Lifecycle events (FR-020): `session.opened`, `session.loaded`, `session.refreshed`,
  `session.closed`, `session.expired`, `session.evicted`, `session.load_failed`.
  [NEEDS CLARIFICATION: sink — logs/metrics only, or also a NoETL event/callback?]

## Guarantees

- **Single writer per instance**: one primary, enforced by `replicas: 1`, the RWO volume
  and the data-dir lock. A second process on the same data refuses to start.
- **Instance isolation**: instances share no state, and an operation on one never changes
  another.
- Writes are acknowledged only after the change log is durable. `graph_version` is
  monotonic per namespace. Replays are no-ops.
- A session reads exactly one `snapshot_version` until a refresh. A refresh never yields
  a read that mixes versions.
- There is one resident copy per session. Identical copies may be shared [NEEDS
  CLARIFICATION FR-017].
- When the budget is exceeded, only closed or expired sessions are evicted. An active
  session is never evicted silently.
- **Backup**: the object-store copy is always a consistent prefix of the change log.
  The head pointer is shipped last. RPO = reported backup lag.
- **Not MVP** (research R4, Q18): read replicas or a standby add `role: replica|standby`.
  A replica serves reads at `graph_version` ≤ primary and rejects writes with
  `UNAVAILABLE`, plus a hint pointing to the primary. The tool surface stays the same.
