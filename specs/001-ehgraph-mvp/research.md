# Research: ehgraph MVP

Phase 0 output for `specs/001-ehgraph-mvp`. Carried over from qnetmon research R7–R9
(`specs/001-noetl-network-monitor/research.md`) and re-checked against `noetl/ai-meta`
checkouts on 2026-11-03. Paths are relative to `ai-meta/repos/`.

## R1. Adopt vs build — DECISION: BUILD

**Decision (user, 2026-11-03)**: build a purpose-built graph database, because "we need
graph database anyway". Refined by the user later the same day: ehgraph is a **topology
database**, meaning a separate database server and not a library. It serves NoETL
services and cluster topology, network topology, and other topologies, with one instance
per purpose (R2 decision, R4). Expected first consumers: a NoETL cluster topology and
qnetmon (network).

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
yes, push back hard") both work against a long-lived service.

**DECISION (user, 2026-11-03): SERVICE.** The user's words: "it should be a separate
database - not a library. single instance - so we can have many instances if needed for
different purposes."

- ehgraph is a standalone topology database server. Each instance is a persistent
  `kind: Mcp` server.
- This is recorded as an **accepted exception** to both rules, in the constitution
  (2.0.0, "Accepted Exceptions") and in plan.md Complexity Tracking.

Why the exception is justified:

1. **Persistent state.** ehgraph is a database, like PostgreSQL. Its state must outlive
   sessions, executions and workers. It is not an app microservice that could be
   ephemeral.
2. **Shared session memory.** Shared-memory session graphs need one long-lived holder
   that every reader can reach. Gaps 1–4 above rule out workers and the IPC cache.
3. **Separate domains.** Separate instances per domain need a deployable unit of their
   own. Embedding in NoETL server or worker would couple topology storage to NoETL's
   release and scaling.

Conditions that keep the exception narrow:

- no external DB or coordination dependency per instance
- lifecycle owned by catalog playbooks
- session graphs bounded and derived
- playbooks and workers hold no ehgraph state

The library crates remain as internal structure only. Q14 is closed.

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

## R4. Instance model — single instance vs replicated vs distributed

**User direction (2026-11-03)**: "single instance - so we can have many instances if
needed for different purposes. not sure though what design is better single instance or
distrbuted multiple instance or replicated."

Every option below starts from **many independent instances, one per topology domain**
(e.g. `ehgraph-noetl-cluster`, `ehgraph-network`). Each instance has its own volume,
object-store prefix and `kind: Mcp` resource, and instances share nothing. The open
question is the shape of **one instance**:

- **(a) single instance**: one process is the only writer and reader. It has a durable
  change log, snapshots, and backup/restore through an object store.
- **(b) single primary + read replicas or warm standby**: one writer, plus followers that
  tail the change log. Followers serve reads and session graphs, and a standby can be
  promoted.
- **(c) distributed**: the graph is sharded across nodes, and/or multiple writers accept
  writes.

### What EHDB already offers (checked in `repos/ehdb`, checkout 8b09a0f)

| Piece | Where | State in the code | What ehgraph could reuse |
|---|---|---|---|
| Change feed `Watch(shard, cursor)` | `crates/ehdb-l0/src/feed.rs` (`ChangeFeed::poll`) | in-process primitive over an immutable, sort-key-ordered log; exact resume from a cursor | the **log-shipping shape** for (b): a follower is a subscriber with a cursor (`graph_version`) |
| Networked feed | `crates/ehdb-feed` (`FeedWriter::append`, `SubscribeReq{shard,cursor}`) | **T0 shadow transport**: length-prefixed JSON over TCP, push via a watch channel, resume by `global_sequence`. NATS stays authoritative | design/code reference for replica streaming. Not production-authoritative in EHDB yet |
| Writer election | `crates/ehdb-reference/src/election.rs` (`LeaseStore`, `LeaseRecord`, monotonic epoch) | state machine proven against a CAS store. Self-described "**Wired, but NOT authoritative**". The **Kubernetes Lease adapter is not implemented**, see `docs/spec/lease-election-k8s-binding.md` | design for (b) promotion: K8s `coordination.k8s.io` Lease, epoch as fencing token, "Leases and not Raft" |
| Store-side fencing (Invariant F) | `crates/ehdb-reference/src/fencing.rs` (`FencingMode::{Shadow,Enforce}`) | ships in **Shadow** (counts, refuses nothing) by default | the rule (b) needs: the store refuses an append whose epoch is below the highest accepted |
| Write failover | `crates/ehdb-l0/src/write_failover.rs` (`FailoverMode`) | **scaffold**: `activate` refuses everything except `Off`. Automatic failover is "not recommended" (split-brain risk) | supports manual or owner-gated promotion only |
| Replica targets / membership / placement | `ehdb-l0/src/replica_targets.rs`, `membership.rs`, `placement.rs` | **scaffolds**: parsing only, read-side only, typed locality | not usable as-is |
| Durability window | `ehdb-l0/src/unreplicated.rs`, `part.rs` (`seal_max_age`), `tests/age_seal.rs` | the ack is fsync'd on **one** disk. A record reaches the durable substrate only when its part seals (8 MiB / 1024 records, or age when enabled; off by default) | the RPO model for (a): the backup lag is the unsealed and unshipped tail, and ehgraph MUST enable an age seal |
| Failure domains / substrates | `ehdb-l0/src/failure_domain.rs`, `substrate.rs`; `ehdb-storage` (`CloudProvider` enum, only `LocalObjectStore` implemented) | `LocalFsSubstrate` is the only storage-backing substrate. A GCS substrate is an open choice (`docs/spec/second-substrate-choice.md`) | ehgraph must provide its own object-store backup target (or wait for EHDB's) |

Two caveats:

- The election and fencing code is in `ehdb-reference`. `noetl/catalog`'s Cargo.toml calls
  that crate "the older parallel lineage… its election/fencing self-describe as
  non-authoritative". Reuse would mean copying or extracting the design, not pinning
  that crate.
- EHDB's own current choice is "**(d) single-writer by construction**"
  (`docs/spec/second-substrate-choice.md`): `StatefulSet replicas: 1` plus a
  `ReadWriteOnce` PVC. Moving to a shared substrate "un-parks the fencing work". That is
  exactly option (a) here, and the same trade-off applies to (b).

### Comparison

| Criterion | (a) single instance | (b) primary + replicas / standby | (c) sharded / multi-writer |
|---|---|---|---|
| Topology graph size (10^3–10^6 elements per domain) | fits in one node's memory, with headroom for sessions | same data on each node | adds capacity that is not needed |
| Traversal locality (k-hop, shortest path) | all local, in-memory CSR | all local on any node | **cross-partition hops**: network round trips per hop. Topology graphs (meshes, dependency DAGs) partition badly |
| Session memory (FR-010–020) | one host holds all sessions. Bounded by one node's budget | sessions spread over replicas; a session pins to one node. Replica lag affects `refresh` freshness | a session's scope may span shards, so it needs a distributed load and consistent cut |
| Consistency (II, III) | trivially one ordered log per namespace | still one log and one writer. Replicas are versioned (`graph_version` ≤ primary) | needs distributed ordering or multi-writer conflict rules. Conflicts with "one ordered log" |
| Failover / RPO / RTO | RPO = backup lag (unshipped tail). RTO = reschedule pod and reattach volume (minutes), or restore from the object store plus rebuild (minutes, size-bound). Writes and reads are down meanwhile | standby RPO ≈ replication lag (seconds). RTO = promote (seconds to a minute) once election and fencing exist. Reads continue on replicas | complex. Partial availability. Hardest to reason about |
| Operational cost | lowest: one pod, one volume, one prefix | 2–3 pods, election (K8s Lease + RBAC), fencing, lag monitoring | highest: a coordinator, rebalancing, distributed tests |
| EHDB reuse | substrate, parts, manifest, object store (R3). Age-seal posture | adds the feed shape plus the election/fencing **designs**. Not production-ready in EHDB | none |
| Self-sufficiency footprint | minimal | moderate | large |

### Recommendation (staged roadmap)

1. **Stage 0 — MVP: (a), one single-primary instance per domain.**
   - durable change log and snapshots (II, III)
   - age-bounded seal plus continuous backup of sealed parts, snapshots and manifest to
     an object store (FR-024)
   - restore into an empty volume
   - single writer by construction: `StatefulSet replicas: 1`, RWO volume and a
     data-dir lock (FR-025)
   - target: RPO = backup lag, RTO = reattach, or restore plus rebuild
2. **Stage 1 — next: (b), primary plus read replicas, then warm standby.**
   - Followers tail the change log from a cursor (`graph_version`), in the shape of the
     EHDB feed, and can serve sessions.
   - Promotion is **manual / owner-gated** first, using a K8s Lease election with
     epoch fencing that the store enforces (Invariant F).
   - Triggers for this stage: a stated RTO under the reattach time, or a read/session load
     that exceeds one node.
3. **Deferred — (c).** The reasons:
   - Traversals across partitions destroy the locality that k-hop and path queries rely
     on.
   - Topology graphs per domain fit in memory. Scaling out is done by **more instances
     per domain**, not by sharding.
   - Multi-writer breaks the one-ordered-log principle (II).

   Revisit only with a measured domain that does not fit on one node.

The **final choice stays open**: Q18 below. The recommendation is Stage 0 for the MVP,
with Stage 1 planned.

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
| ~~Q14~~ | ~~Service vs embedded library~~ — **RESOLVED: service** (standalone topology database server; accepted exception; constitution 2.0.0) | R2 |
| Q15 | EHDB L0 reuse terms and stable API | R3 |
| Q16 | Language (Rust), licence | constitution |
| Q17 | Scale and SLO targets per domain | SC-003, SC-004, Assumptions |
| Q18 | Final instance model: (a) single, (b) primary + replicas/standby, (c) distributed. **Recommended: (a) for the MVP, (b) next, (c) deferred** | R4, FR-025 |
| Q19 | RPO/RTO per domain; object store for backups (GCS, S3-compatible, mounted volume) | FR-024, SC-008 |
| Q20 | Initial domains and instance names (proposed: `noetl-cluster`, `network`) | FR-022, FR-023 |
