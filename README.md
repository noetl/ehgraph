# ehgraph — Event Horizon Graph

A property-graph database for the NoETL ecosystem. It provides:

- a durable change log
- versioned snapshots
- session-scoped shared-memory graphs
- access through MCP tools and HTTP, so NoETL playbooks and agents can use it

Status: **specification only** (spec-kit). See:

- Constitution: [.specify/memory/constitution.md](.specify/memory/constitution.md)
- MVP spec: [specs/001-ehgraph-mvp/](specs/001-ehgraph-mvp/spec.md)
- Wiki: <https://github.com/noetl/ehgraph/wiki>

First consumer: qnetmon (network topology graph).

ehgraph reuses EHDB storage crates as libraries. It never stores graph data in EHDB.
