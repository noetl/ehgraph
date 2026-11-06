# Implementation Plan: ehgraph MVP

**Branch**: `001-ehgraph-mvp` (git `spec/initial`) | **Date**: 2026-11-03 |
**Spec**: [spec.md](./spec.md)

## Summary

ehgraph is a **topology database server**: a standalone database, not a library. It
serves topology graphs such as NoETL services/cluster topology and network topology. Each
topology domain gets its own single-primary instance (e.g. `ehgraph-noetl-cluster`,
`ehgraph-network`). Each instance provides:

- a durable change log with per-namespace `graph_version`
- versioned immutable snapshots
- fixed read/traversal operations
- session-scoped, read-only in-memory graphs bounded by lease and budget
- access through MCP tools and HTTP, with each instance registered as its own NoETL
  `kind: Mcp` resource
- backup and restore of the change log and snapshots through an object store

The instance-model roadmap is in research R4:

- MVP: single primary per domain
- next: read replicas / warm standby fed by the change log
- deferred: sharding

The final choice is Q18.

Storage reuses EHDB L0 crates as tag-pinned git libraries. Graph data never goes into
EHDB.

## Technical Context

**Language/Version**: Rust edition 2021, toolchain 1.99.1 (provisional; see research.md "Provisional decisions", Q16). The implementation request confirmed Rust. The MSRV recorded in `rust-toolchain.toml` is 1.99.1 because current crates.io releases need it.

**Primary Dependencies**:

- `ehdb-l0` (and `ehdb-storage` if used) as a git dependency pinned to one ehdb tag.
  Both pins move together, as in catalog and signal-mesh.
- `arrow` (record batches, IPC)
- `tokio`
- an HTTP framework (`axum` proposed)
- `serde`/`serde_json`
- an MCP JSON-RPC layer: [PROVISIONAL C24, research.md register: hand-rolled or a crate]
- `prometheus` metrics

**Storage**: each instance has its own change log and snapshots on a `DurableSubstrate`
(`LocalFsSubstrate` on an RWO volume), with an age-bounded seal. Sealed parts, snapshots
and the manifest are shipped to an object store under `<backup_prefix>/<instance_id>/`.
There is no external DB. [PROVISIONAL C19, research.md register Q19: the object store client. EHDB has only
`LocalObjectStore`, and its GCS substrate is undecided.]

**Testing**:

- `cargo test` for unit tests and crash/rebuild tests.
- Fixture graphs under `tests/fixtures/`.
- A contract test via a NoETL playbook (`kind: mcp`) on the local runtime.
- `criterion` benchmarks for load latency and footprint.

**Target Platform**: Linux container on Kubernetes (local kind first). Also a local
process for laptops. [PROVISIONAL C06, research.md register FR-009]

**Project Type**: a database server. It is built as a Rust workspace whose internal
crates produce one `ehgraph` server binary and container image. The crates are not a
supported embedding API.

**Performance Goals**: SC-003/SC-004. [PROVISIONAL C17, research.md register: targets]

**Constraints**:

- one primary (single writer) per instance: `StatefulSet replicas: 1`, RWO volume and a
  data-dir lock (FR-025)
- instances share nothing
- bounded memory (FR-016)
- no cross-node shm

**Scale/Scope**: per domain, up to about 10^6 nodes and edges, sized to fit in one node's
memory. Initial domains: `noetl-cluster` and `network` (qnetmon). [PROVISIONAL C17/C20, research.md register
Q17, Q20]

## Constitution Check

| Principle | Status | Notes |
|---|---|---|
| I. Standalone topology database server | PASS | One server binary and image. Consumers use only MCP/HTTP. Generic model, with domain types in consumer schema config. One instance per domain; namespaces isolated inside it. |
| II. Change log is the authority | PASS | Append before acknowledgement. Snapshots, materialized state and sessions are derived. Rebuild test (SC-002). |
| III. Versioned, repeatable reads | PASS | `graph_version` on every result. Pinned session version. Atomic refresh. |
| IV. Bounded session memory | PASS | Idle lease, max lifetime, per-session cap, global budget, explicit overflow. Read-only. |
| V. NoETL-native access | PASS | MCP + HTTP. One `kind: Mcp` resource per instance (`ehgraph-<domain>`), with shared lifecycle playbooks. Keychain auth. |
| VI. Reuse libraries, own the state | PASS | EHDB L0 by git tag pin; no data in EHDB; no external DB or coordination service per instance. |
| Instance Model | PASS | MVP single primary per domain, plus backup/restore. Replicas only after election and store-side fencing (R4). |
| Accepted Exceptions | PASS (documented) | Separately-running persistent server vs self-sufficiency and the ephemeral principle. Accepted by the user; see Complexity Tracking. |
| VII. Test failure conditions | PASS | Crash-between-append-and-ack, rebuild, restore from backup, second process on the same data dir, concurrent first load, idle expiry, budget overflow, refresh during reads, instance isolation. |

## Project Structure

### Documentation (this feature)

```text
specs/001-ehgraph-mvp/
├── spec.md
├── plan.md
├── research.md        # R1 build, R2 NoETL facts + SERVICE decision, R3 EHDB reuse, R4 instance model
├── data-model.md
├── contracts/graphdb.md
└── tasks.md
```

### Source Code (repository root, proposed)

```text
Cargo.toml                 # workspace; ehdb-l0 pinned by tag
crates/
├── ehgraph-core/          # model, ids, typed props, ChangeRecord, graph_version
├── ehgraph-store/         # change log + snapshots + manifest on EHDB L0; rebuild; upserts; data-dir lock
├── ehgraph-backup/        # object-store shipper (sealed parts, snapshots, manifest, head last) + restore
├── ehgraph-api/           # read/traversal ops over the durable store or a session graph
├── ehgraph-memory/        # session registry, loader, Arrow + CSR graph, budgets, leases
└── ehgraph-server/        # the `ehgraph` binary: instance config, MCP JSON-RPC + HTTP, auth, metrics, health
playbooks/                 # kind: Mcp resource template + one resource per instance; shared lifecycle
                           # playbooks (instance-parameterized); ops/backup, ops/restore; contract tests
deploy/                    # container image + per-instance StatefulSet/Service/config templates
tests/fixtures/            # fixture graphs + expected results
benches/
```

**Structure Decision**: the product is the `ehgraph` database server (Q14 resolved:
SERVICE).

- The workspace crates (`core`, `store`, `backup`, `api`, `memory`) are internal modules
  for separation and testing. They are not published, and they are not an embedding API.
- One binary runs one instance. Its configuration sets `instance_id`, `domain`,
  `data_dir`, `backup_prefix` and budgets.
- Multiple domains mean multiple deployments of the same image.
- Stage 1 replicas (R4) would add a `role` and a log-tail follower inside the same
  binary. They need no new crate boundary.

## Complexity Tracking

| Violation | Why Needed | Simpler Alternative Rejected Because |
|---|---|---|
| **Accepted exception 1**: a separately-running database server (`ehgraph`, one per domain), despite ai-meta `agents/rules/self-sufficiency.md` ("push back hard" on new services). Accepted by the user on 2026-11-03 ("a separate database - not a library") | A topology database's state must outlive sessions, executions and workers, like PostgreSQL; it is not an app microservice. Session residency (FR-013) needs one long-lived holder reachable by every reader; NoETL workers are stateless and the IPC cache is same-node with 60 s leases (R2). Separate instances per domain need their own deployable unit. | Reloading from storage on every step fails FR-012/FR-015. Embedding in NoETL server or worker couples topology storage to NoETL releases and scaling, and rules out per-domain instances. An external graph DB is a bigger self-sufficiency breach (R1). |
| **Accepted exception 2**: persistent `kind: Mcp` server instances, despite the NoETL ephemeral principle (`ephemeral_blueprints.md`: "No persistent MCP server instances") | Same as above: ehgraph is storage, not an ephemeral workload or tool session. | Session-lived instances would lose the durable state or reload it per session, which defeats the database. Conditions: bounded and derived session graphs; lifecycle owned by catalog playbooks; workers and playbooks hold no ehgraph state. |
| Many single-primary instances instead of one shared cluster | Per-domain isolation of data, blast radius, budgets and release cadence (FR-022). The instance is the scale-out unit (R4). | A multi-tenant shared instance couples unrelated domains. Sharding loses traversal locality (R4). |
| A git dependency on EHDB internals (`ehdb-l0`) outside the EHDB program | Durable substrate, parts, frames and manifest are already written and tested (R3). | Writing a new log and manifest duplicates subtle durability code. Adopting an external DB violates self-sufficiency (R1). |
