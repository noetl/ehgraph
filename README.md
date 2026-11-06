# ehgraph — Event Horizon Graph

A **topology database** for the NoETL ecosystem. It stores topology graphs and runs in two
modes from one engine: **standalone** (the `ehgraph` server) or **embedded** in-process in a
host such as noetl-server (the `ehgraph` crate). Embedded mode is proposed and not yet
ratified; see [docs/wiki/Embedding.md](docs/wiki/Embedding.md). Examples:

- NoETL services and cluster topology
- network topology
- other topologies

Each server instance provides:

- a durable change log
- versioned snapshots
- session-scoped shared-memory graphs
- access through MCP tools and HTTP, so NoETL playbooks and agents can use it
- backup and restore through an object store

Run one instance per purpose or topology domain, for example `ehgraph-noetl-cluster`
and `ehgraph-network`. Each instance is a single primary, and is registered as its own
NoETL `kind: Mcp` resource. Read replicas are the planned next stage, and sharding is
deferred (research R4).

Status: **MVP server** on top of the spec in `specs/001-ehgraph-mvp/`. One `ehgraph` binary serves one primary instance. Read replicas are not implemented.

```bash
cargo build -p ehgraph-server
scripts/ehgraph-local.sh start network          # 127.0.0.1:8081
scripts/ehgraph-local.sh start noetl-cluster    # 127.0.0.1:8082
curl -s http://127.0.0.1:8081/healthz
```

- Constitution: [.specify/memory/constitution.md](.specify/memory/constitution.md)
- MVP spec: [specs/001-ehgraph-mvp/](specs/001-ehgraph-mvp/spec.md)
- User and developer docs: [docs/wiki/Home.md](docs/wiki/Home.md) (GitHub wiki page names, kept in-repo)
- Wiki: <https://github.com/noetl/ehgraph/wiki>

Expected first consumers: a NoETL cluster topology (`ehgraph-noetl-cluster`), and qnetmon (`ehgraph-network`).

EHDB is a design reference for the change-log frame, sealed parts, and age seal. This repository does not depend on it, and it never stores graph data in an EHDB dataset. See `specs/001-ehgraph-mvp/research.md` (R3 implementation note, Q15).
