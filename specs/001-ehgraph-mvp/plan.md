# Implementation Plan: ehgraph MVP

**Branch**: `001-ehgraph-mvp` (git `spec/initial`) | **Date**: 2026-11-03 |
**Spec**: [spec.md](./spec.md)

## Summary

ehgraph is a property-graph store for the NoETL ecosystem. It provides:

- a durable change log with per-namespace `graph_version`
- versioned immutable snapshots
- fixed read/traversal operations
- session-scoped, read-only in-memory graphs bounded by lease and budget
- access through MCP tools and HTTP, deployed as a NoETL `kind: Mcp` resource

Storage reuses EHDB L0 crates as tag-pinned git libraries. Graph data never goes into
EHDB.

## Technical Context

**Language/Version**: Rust (stable, edition 2021) [NEEDS CLARIFICATION: confirm; MSRV]

**Primary Dependencies**:

- `ehdb-l0` (and `ehdb-storage` if used) as a git dependency pinned to one ehdb tag.
  Both pins move together, as in catalog and signal-mesh.
- `arrow` (record batches, IPC)
- `tokio`
- an HTTP framework (`axum` proposed)
- `serde`/`serde_json`
- an MCP JSON-RPC layer: [NEEDS CLARIFICATION: hand-rolled or a crate]
- `prometheus` metrics

**Storage**: own change log and snapshots on `DurableSubstrate` (`LocalFsSubstrate`
first; object store later). There is no external DB.

**Testing**:

- `cargo test` for unit tests and crash/rebuild tests.
- Fixture graphs under `tests/fixtures/`.
- A contract test via a NoETL playbook (`kind: mcp`) on the local runtime.
- `criterion` benchmarks for load latency and footprint.

**Target Platform**: Linux container on Kubernetes (local kind first). Also a local
process for laptops. [NEEDS CLARIFICATION FR-009]

**Project Type**: Rust workspace (library crates plus a thin server binary).

**Performance Goals**: SC-003/SC-004. [NEEDS CLARIFICATION: targets]

**Constraints**: single node per namespace; bounded memory (FR-016); no cross-node shm.

**Scale/Scope**: set by the first consumer (qnetmon). [NEEDS CLARIFICATION]

## Constitution Check

| Principle | Status | Notes |
|---|---|---|
| I. Consumer-agnostic | PASS | Generic model. Consumer types live in consumer schema config. Namespaces are isolated. |
| II. Change log is the authority | PASS | Append before acknowledgement. Snapshots, materialized state and sessions are derived. Rebuild test (SC-002). |
| III. Versioned, repeatable reads | PASS | `graph_version` on every result. Pinned session version. Atomic refresh. |
| IV. Bounded session memory | PASS | Idle lease, max lifetime, per-session cap, global budget, explicit overflow. Read-only. |
| V. NoETL-native access | PASS | MCP + HTTP. `kind: Mcp` resource + lifecycle playbooks. Keychain auth. |
| VI. Reuse libraries, own the state | PASS (conditional) | EHDB L0 by git tag pin; no data in EHDB; no external DB. A separately-running service conflicts with self-sufficiency → Complexity Tracking + Q14. |
| VII. Test failure conditions | PASS | Crash-between-append-and-ack, rebuild, concurrent first load, idle expiry, budget overflow, refresh during reads. |

## Project Structure

### Documentation (this feature)

```text
specs/001-ehgraph-mvp/
├── spec.md
├── plan.md
├── research.md        # R1 build decision, R2 NoETL shm facts and gaps, R3 EHDB reuse
├── data-model.md
├── contracts/graphdb.md
└── tasks.md
```

### Source Code (repository root, proposed)

```text
Cargo.toml                 # workspace; ehdb-l0 pinned by tag
crates/
├── ehgraph-core/          # model, ids, typed props, ChangeRecord, graph_version
├── ehgraph-store/         # change log + snapshots + manifest on EHDB L0; rebuild; upserts
├── ehgraph-api/           # read/traversal ops over the durable store or a session graph
├── ehgraph-memory/        # session registry, loader, Arrow + CSR graph, budgets, leases
└── ehgraph-server/        # MCP JSON-RPC + HTTP, auth, metrics, health (binary)
playbooks/                 # kind: Mcp resource + lifecycle playbooks; contract-test playbook
deploy/                    # container image + k8s manifests used by lifecycle playbooks
tests/fixtures/            # fixture graphs + expected results
benches/
```

**Structure Decision**: the core is in library crates (`core`, `store`, `api`, `memory`)
so it can be embedded by NoETL server or worker, which keeps Q14 open. The server crate
is a thin adapter.

## Complexity Tracking

| Violation | Why Needed | Simpler Alternative Rejected Because |
|---|---|---|
| A separately-running service (`ehgraph-server`) despite ai-meta self-sufficiency ("push back hard" on new services) | Session residency (FR-013) must outlive individual playbook steps. NoETL workers are stateless, and the IPC cache has 60 s, same-node, age-evicted leases (research R2). | Reloading from durable storage on every step fails FR-012/FR-015. Embedding in NoETL server is viable later (library crates), but needs NoETL server changes (Q14). |
| A git dependency on EHDB internals (`ehdb-l0`) outside the EHDB program | Durable substrate, parts, frames and manifest are already written and tested (R3). | Writing a new log and manifest duplicates subtle durability code. Adopting an external DB violates self-sufficiency (R1). |
