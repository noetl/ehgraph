# ehgraph

ehgraph (Event Horizon Graph) is a **topology database**. You run one instance per purpose, either as the standalone server (connect over MCP or HTTP) or embedded in-process in a host such as noetl-server ([Embedding](Embedding)). The graph for a user session stays resident in that process until the session ends.

Examples:

- `ehgraph-network` — network topology, the store qnetmon reads
- `ehgraph-noetl-cluster` — NoETL services and cluster topology

Each instance is a single primary in this release. It has its own change log, snapshots, and object-store backup. Instances do not know about each other. Read replicas are planned and not built. Sharding is deferred.

## What you get

- A durable, ordered change log. Every acknowledged write has a `graph_version` for that namespace.
- Immutable snapshots, and reads that say which version they reflect.
- A session graph: the scope you asked for is loaded once into memory and stays pinned until you close the session or it goes idle.
- The same tools over MCP (`tools/list`, `tools/call`) and HTTP (`POST /v1/tools/{tool}`).
- Backup and restore of sealed log parts, snapshots, and the manifest. The head pointer is shipped last.

## Where to go

- [Getting Started](Getting-Started) — config, a laptop process, one instance per domain
- [Architecture](Architecture) — instance, namespace, change log, sessions
- [API and MCP](API-and-MCP) — tools, the result envelope, errors
- [Operations](Operations) — health, metrics, backup, Kubernetes
- [Developer Guide](Developer-Guide) — crates, build, test, contributing
- [Roadmap](Roadmap) — replicas later, sharding not now
- [Tool contract](Tool-Contract) — the MVP tool surface

Source and spec: the `noetl/ehgraph` repository, `specs/001-ehgraph-mvp/`.
