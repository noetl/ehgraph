# Research: ehgraph MVP

Phase 0 output for `specs/001-ehgraph-mvp`. Carried over from qnetmon research R7–R9
(`specs/001-noetl-network-monitor/research.md`) and re-checked against `noetl/ai-meta`
checkouts on 2026-11-03. Paths are relative to `ai-meta/repos/`.

## R1. Adopt vs build — DECISION: BUILD

**Decision (user, 2026-11-03)**: build a purpose-built graph database, because "we need
graph database anyway". ehgraph is a reusable NoETL ecosystem component, and qnetmon is
its first consumer.

Supporting reasons:

- **Session layer.** The session-scoped shared-memory requirement (FR-010–FR-020) needs
  snapshot versioning, bounded scope export and residency control. None of the adopt
  options gives these directly. With an adopted DB, the session layer would still have to
  be built.
- **Existing storage pieces.** NoETL already has the pieces of a storage engine (EHDB L0
  parts, manifest and substrate, R3) and an Arrow IPC data plane (R2). "Build" is
  therefore mostly assembly plus a graph-specific layer.
- **Native NoETL access.** A NoETL-owned store can expose itself natively to playbooks and
  agents (MCP, and maybe a tool kind) rather than through a third-party driver.

Background comparison (kept for the record; facts about third-party products are general
knowledge and were not re-verified):

| Option | NoETL reach today | For | Against |
|---|---|---|---|
| PostgreSQL node/edge tables | `kind: postgres` | no new infra, transactions, recursive CTEs | traversal in SQL; no snapshot/session layer; external DB (self-sufficiency) |
| PostgreSQL + Apache AGE | `kind: postgres` | openCypher in Postgres | extension availability; no session layer |
| Neo4j | `http` / MCP server | mature, Cypher | separate cluster; licensed enterprise features; no NoETL session integration |
| Memgraph | `http` / Bolt via MCP | in-memory, Cypher | separate service; memory model is per DB, not per session |
| DuckDB + DuckPGQ | `kind: duckdb` | embedded, SQL/PGQ | extension maturity; single-writer files; not a serving store |
| **Purpose-built (ehgraph)** | MCP + `http` first | snapshot/session design fits; reuses EHDB and Arrow pieces | build and operate cost; must stay scoped to the MVP |

**MVP scope**: FR-001–FR-020. Out of MVP: a general query language, replication,
multi-request transactions, and algorithms beyond traversal and shortest path.

## R2. Session-scoped shared memory — what NoETL already has

Verified in local checkouts:

- **Arrow IPC shared-memory cache** — `cli/arrow-cache` (`noetl-arrow-cache`, the Rust
  mirror of Python `noetl.core.storage.ipc_cache.ArrowIpcSharedMemoryCache`).
  - Uses POSIX `shm_open` + `mmap`, keyed by a JSON `IpcHint`.
  - Default lease is 60 s. `NOETL_IPC_CACHE_BUDGET_BYTES` is a soft budget (256 MB
    default) with oldest-by-lease eviction.
  - Consumers refuse to attach when the producer `node_id` differs.
  - Used by the worker to stage `call.done` results.
- **Distributed runtime spec** (`docs/docs/features/noetl_distributed_runtime_spec.md`
  §6). Tier 1.5 is "Arrow IPC over POSIX shm/memfd", per host, with lifetime "frame lease
  + 30 s grace". It is always backed by a durable copy. References that exist only in IPC
  are rejected.
- **Ephemeral principle** (`docs/docs/architecture/ephemeral_blueprints.md`): "No
  persistent MCP server instances".
- **`kind: Mcp` catalog resources** (`docs/docs/architecture/mcp_catalog_architecture.md`,
  noetl ≥ 2.26): `spec.url`, `spec.protocol`,
  `spec.lifecycle.{deploy,undeploy,redeploy,restart,status,discover}` playbooks,
  `spec.discovery`, `spec.deployment`. This is the shipped pattern for a NoETL-managed
  MCP service.
- **Playbook-as-MCP-Server** (`POST /api/mcp/playbook/{path}/jsonrpc`).
- **Sessions**: the gateway issues tokens with `expires_at`. Logout deactivates a session.
  There is **no session-end hook** that triggers a playbook.
- **KV**: NoETL's internal NATS KV buckets were removed (ai-meta#212).

Gaps that ehgraph must fill:

1. No session-bound residency. IPC leases are short and evict by age.
2. Same-node only. Remote readers need a network API.
3. The IPC cache moves opaque bytes. It has no graph indexes or traversal.
4. No session-end hook, so ehgraph needs its own close plus idle timeout.
5. The IPC cache is internal to the worker. It is not a playbook tool.
6. No documented way for `noetl run --runtime local` to start or stop a session-lived
   local service. [NEEDS CLARIFICATION]

**Design consequence**: a graph-memory component holds each session's snapshot in memory
and serves reads over MCP/HTTP. The snapshot is held as Arrow record batches plus a CSR
adjacency index, with optional POSIX shm placement in the `IpcHint` format. The durable
store stays the authority.

**Tension**: the ephemeral principle ("no persistent MCP server instances") and ai-meta
`agents/rules/self-sufficiency.md` ("Does this need to be a separate running service? If
yes, push back hard") both work against a long-lived service. Mitigations:

- The service is session-lived and deployed by lifecycle playbooks.
- The core is a library (`ehgraph-core`, `-store`, `-memory`), so NoETL server or worker
  could embed it instead.

[NEEDS CLARIFICATION: service, embedded, or both for the MVP.]

## R3. Reusing EHDB storage pieces — checked against `repos/ehdb` (checkout 8b09a0f)

| Piece | Where | What it does | Fit |
|---|---|---|---|
| `DurableSubstrate` + `LocalFsSubstrate` | `crates/ehdb-l0/src/substrate.rs` | durable byte-sink: `put_if_absent` (immutable), `put_overwrite` (pointers), ranged reads | **Good fit** for snapshot parts and the manifest pointer |
| Immutable parts, `PartWriter`/`SealedPart`, frames | `crates/ehdb-l0/src/part.rs`, `frame.rs` (`magic+len+crc32+body`) | append, seal, fsync postures | Good fit for the change log |
| `Manifest`/`PartMeta`/`SparseIndex` | `crates/ehdb-l0/src/catalog.rs` | versioned manifest; readers see a consistent snapshot | **Good fit** for versioned snapshots |
| `Dataset` trait | `crates/ehdb-l0/src/dataset.rs` | engine generic over a dataset: `Record`, `sort_key`, `partition`, `index_key` | Change-log shape fits (sort key = `graph_version`, index key = element id) |
| Merge/compaction | `crates/ehdb-l0/src/merge.rs` | small→big merge per partition | Possible for log compaction |
| Columnar codec | `crates/ehdb-l0/src/columnar.rs` | per-field column sections | Pattern only (specific to D1) |
| `ImmutableObjectStore` / `LocalObjectStore` | `crates/ehdb-storage/src/lib.rs` | content-addressed (sha256) objects | Fit for snapshot files |

Constraints:

- **Program invariant** (`ehdb-l0/src/lib.rs`): EHDB is a NoETL-internal store over a
  fixed set of predefined datasets. "Business/domain data and secret values are NEVER in
  EHDB." ehgraph is therefore a **separate store** that links these crates. It never
  runs inside EHDB and never writes to EHDB datasets.
- **How the crates are consumed** (corrects the qnetmon note): they are not on
  crates.io. The established precedent is a **git dependency pinned by tag**:
  - `catalog/Cargo.toml`: `ehdb-l0` + `ehdb-core`, `tag = "v0.4.5"`, with the comment
    "Both pins MUST move together".
  - `signal-mesh/Cargo.toml`: `ehdb-core` + `ehdb-l0`, `tag = "v0.4.1"`.
  - ehgraph follows the same pattern: `ehdb-l0` (and `ehdb-storage` if used) pinned to
    one tag, moved together. The latest tag at checkout time was v0.14.2.
  - The workspace licence is Apache-2.0.
- **Embedding the `Dataset` engine**: adding a dataset is "a deliberate compiled-in
  change" in EHDB's model. ehgraph should use the substrate, part, frame and manifest
  layer directly, or define its own `Dataset` impl in its own crate. Either way it should
  confirm with EHDB maintainers that L0 may serve a store outside EHDB, and which APIs are
  stable. [NEEDS CLARIFICATION]

## Open questions (collected)

| # | Question | Ref |
|---|---|---|
| Q1 | Schema per namespace or schemaless | FR-001 |
| Q2 | Snapshot cadence and retention | FR-003 |
| Q3 | Fixed operations vs openCypher/GQL subset | FR-005 |
| Q4 | First-class NoETL tool kind (`kind: graph`) | FR-006 |
| Q5 | Auth token type; NoETL server auth integration | FR-008 |
| Q6 | MVP deploy targets (K8s, local process) | FR-009, R2 gap 6 |
| Q7 | Owner of a graph session | FR-010 |
| Q8 | Scope granularity for the MVP | FR-011 |
| Q9 | Idle timeout, maximum lifetime, behaviour after expiry | FR-014 |
| Q10 | Freshness model (pinned + refresh vs deltas) | FR-015 |
| Q11 | Budget sizes and overflow response | FR-016 |
| Q12 | Shared copies across sessions | FR-017 |
| Q13 | Session-local what-if edits | FR-019 |
| Q14 | Service vs embedded library (self-sufficiency) | R2 |
| Q15 | EHDB L0 reuse terms and stable API | R3 |
| Q16 | Language (Rust), licence | constitution |
| Q17 | Scale and SLO targets | SC-003, SC-004, Assumptions |
