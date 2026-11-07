# Tool contract

The normative list is `specs/001-ehgraph-mvp/contracts/graphdb.md`. This page is the operator's short copy. Behaviour that the code had to choose is marked provisional and written up in `specs/001-ehgraph-mvp/research.md`.

## Resource

| Field | Value |
|---|---|
| `metadata.name` | `ehgraph-<domain>` |
| `metadata.path` | `mcp/ehgraph/<domain>` |
| Service | `ehgraph-<domain>.ehgraph.svc.cluster.local:8080` |
| MCP URL | `http://ehgraph-<domain>.ehgraph.svc.cluster.local:8080/mcp` |
| Health | `.../healthz` |
| Tool list | `.../mcp/tools` |

That table is for a standalone domain. An embedded domain is served by its host: `ehgraph-noetl-cluster` is at `http://noetl.noetl.svc.cluster.local:8082/ehgraph/noetl-cluster/mcp`. Its resource sets `spec.deployment.mode: embedded`, and it has only the `status` and `discover` verbs (see [Embedding](Embedding)).

Initial resources: `ehgraph-network` (standalone) and `ehgraph-noetl-cluster` (embedded in noetl-server). Files are under `playbooks/automation/agents/ehgraph/resources/`.

Lifecycle playbooks are shared and read `workload.mcp_resource`, which the NoETL lifecycle dispatcher supplies. Source: `playbooks/automation/agents/ehgraph/lifecycle/`. Backup and restore are separate playbooks under `ops/` because `kind: Mcp` has no backup verb.

## Tools

Session: `graph.session.open`, `graph.session.refresh`, `graph.session.close`, `graph.session.info`, `graph.session.list`.

Read: `graph.node`, `graph.edge`, `graph.query`, `graph.neighbors`, `graph.subgraph`, `graph.path`, `graph.snapshot.info`.

Write: `graph.upsert`, `graph.deactivate`, `graph.snapshot.create`. Rejected with `READ_ONLY_SESSION` when `graph_session_id` is set.

Instance: `graph.instance.info`, `graph.backup.status`.

Details, filters, and error codes: [API and MCP](API-and-MCP).

## Guarantees the tests lock

- One batch is one version, or no version if nothing changed.
- A crash after the frame is durable does not leave half a batch.
- Rebuild from the log and snapshots matches the previous answers.
- A second process on the same data directory does not start.
- A session reads one version until refresh, including a read that overlaps the refresh.
- Closing or idling a session makes the id return `SESSION_EXPIRED`.
- Budget pressure never drops an active session.
- Restore reproduces the backed-up version. A second instance is unchanged.
