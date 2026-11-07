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
   local service. [PROVISIONAL C06, research.md register]

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
  stable. [PROVISIONAL C15, research.md register]

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

## R3 implementation note (2026-11-04) — local substrate, EHDB not linked

Q15 is still open: EHDB maintainers have not confirmed that L0 may be linked by a store outside EHDB, and the election/fencing crates are scaffolds. This implementation does **not** depend on `ehdb-l0` or `ehdb-storage`.

What was built instead, so the boundary stays recognizable:

- Frames are `magic (EHGR) || version || u32 length || crc32 || JSON body`.
- Parts are immutable once sealed. Seal triggers are size (8 MiB), count (1024 records), or age (30s, enabled). EHDB leaves age seal off; ehgraph turns it on.
- A versioned manifest lists sealed parts and snapshots. The head pointer is a separate file, published after the frame is fsynced.
- Snapshots are Arrow IPC files. Backup copies sealed parts with put-if-absent semantics and writes the head last, under `backup_prefix` (a directory).
- Nothing is written to an EHDB instance or dataset. State lives under the instance `data_dir`.

A later decision to pin `ehdb-l0` can replace the frame and substrate code. The `graph_version` cursor and the sealed-part layout are the seam a replica would tail. See Roadmap stage 1.

## Provisional decisions (implementation, 2026-11-04)

These are defaults chosen so the MVP could be built. They are not constitution amendments. The open-question table below stays until a human closes each item.

| # | Provisional default |
|---|---|
| Q1 | Schemaless unless `data_dir/schemas/<namespace>.json` exists. `strict: true` rejects unknown types and missing required properties. |
| Q2 | Snapshots on demand via `graph.snapshot.create`. Optional automatic cadence is off (`snapshot_every` is not implemented). Retention keeps the newest 16 snapshots. Log segments are not compacted. |
| Q3 | Fixed tools only. No openCypher or GQL. |
| Q4 | No `kind: graph`. MCP and HTTP only. |
| Q5 | Bearer tokens in the instance config, grants per namespace for read and write. `namespace = "*"` is an operator grant. `auth.mode = "disabled"` for laptops. No call into NoETL `check_playbook_access`. |
| Q6 | Local process (`scripts/ehgraph-local.sh`) and a Kubernetes StatefulSet with `replicas: 1`. |
| Q7 | The session owner is the string the caller passes. ehgraph does not watch gateway session end. |
| Q8 | `full`, `filter` (active nodes matching type/props, plus induced active edges), `khop`, and `snapshot` are all implemented. |
| Q9 | Idle 300s, max lifetime 24h, grace 2s. After expiry the id returns `SESSION_EXPIRED`. No automatic reopen. Reads renew the idle lease only. |
| Q10 | Pinned version plus explicit `graph.session.refresh`. No live deltas. |
| Q11 | Global budget 256 MiB, per-session cap 64 MiB. Evict closed or expired sessions, then `MEMORY_BUDGET_EXCEEDED`. No reduced-scope fallback. |
| Q12 | No cross-session sharing. Single-flight is per `(owner, namespace, scope, requested version)`. |
| Q13 | No what-if edits. A write that carries `graph_session_id` returns `READ_ONLY_SESSION`. |
| Q15 | Do not link EHDB. Local substrate as in the R3 note above. Revisit when maintainers publish stable reuse terms. |
| Q16 | Rust, edition 2021, toolchain 1.99.1. Licence Apache-2.0, matching EHDB, provisional until confirmed. The request to implement in Rust is treated as confirming the language. |
| Q17 | Smoke thresholds only: generated 10k-node / 20k-edge load under 5s. Not a production SLO. The planning assumption remains about 10^6 elements per domain on one node. |
| Q18 | Adopt the R4 recommendation for the code: Stage 0 single primary. No replica process. `role=replica\|standby` refuses to start. |
| Q19 | `backup_prefix` is a filesystem directory (put-if-absent, head last). It can be a mounted bucket later. RPO is the reported lag. No numeric RTO. Default seal age 30s. |
| Q20 | `ehgraph-network` / domain `network`, and `ehgraph-noetl-cluster` / domain `noetl-cluster`. |

Also, so the code could be tested:

- Edge endpoints that are missing or inactive are `INVALID_ARGUMENT`. No placeholder nodes.
- Nested maps and lists are allowed in properties. Timestamps use `{"$timestamp": ...}`.
- Idempotency keys live as long as the namespace log. The hash covers namespace, elements, and `source`, not `execution_id` or `ts_ms`. A different payload for the same key is `CONFLICT`.
- Historical reads start from the newest full snapshot at or below the version, then apply the log tail.
- MCP is a hand-rolled JSON-RPC subset. Metrics are Prometheus text without the client crate, so the label set stays exact.
- Deactivate-by-selector is stored as one op per id, so replay does not re-evaluate the clock.
- Directed shortest path follows active outgoing edges.
- Health and metrics are unauthenticated. Tools require a grant unless auth is disabled.

## Clarification register (2026-11-06) — every open marker, answered provisionally

The user authorized, on 2026-11-06, finishing the build and merging **without** waiting for
ratification. So every `[NEEDS CLARIFICATION]` marker in the spec, plan, data model, contract
and constitution now has a provisional answer here, and the marker in the source points at its
row (`[PROVISIONAL Cnn …]`). Each answer is what the code implements today.

**Revisit** means the owner should look again before the item matters in production. It does
not block anything.
- **yes**: likely to change, or it carries a production risk.
- **low**: a sensible default that is easy to change.

| ID | Question (ref) | Provisional answer, as implemented | Revisit |
|---|---|---|---|
| C01 | Schema per namespace or schemaless (Q1, FR-001) | Schemaless unless `data_dir/schemas/<ns>.json` exists. `strict: true` rejects unknown types and missing required properties. | low |
| C02 | Snapshot cadence and retention (Q2, FR-003) | On demand (`graph.snapshot.create`). Keep the newest 16. Log segments are never compacted. | **yes**: the log grows without bound |
| C03 | Fixed operations vs openCypher/GQL (Q3, FR-005) | Fixed tools only. | low |
| C04 | First-class `kind: graph` (Q4, FR-006) | No. MCP + HTTP, plus the in-process `EhGraph` API in embedded mode. | low |
| C05 | Token type; NoETL auth integration (Q5, FR-008) | Static bearer tokens with read/write grants per namespace in the instance config, identical in both modes. `*` is an operator grant. No `check_playbook_access`. | **yes**: keychain / NoETL auth (E3) |
| C06 | Deploy targets; local runtime (Q6, FR-009, R2 gap 6) | Standalone StatefulSet `replicas: 1`; local process via `scripts/ehgraph-local.sh` (start/stop outside `noetl run`); **embedded** in a host via the `ehgraph` crate. | **yes**: which domains embed (E4) |
| C07 | Session owner (Q7, FR-010) | The string the caller passes. No gateway-session hook. | **yes** (E7) |
| C08 | Scope granularity (Q8, FR-011) | `full`, `filter`, `khop`, `snapshot`. | low |
| C09 | Idle/lifetime; behaviour after expiry; release grace (Q9, FR-014, SC-006) | Idle 300 s, lifetime 24 h, grace 2 s; the sweep runs every 1 s, so memory is released within grace + 1 s. After that, reads with the id return `SESSION_EXPIRED`. No auto-reopen. | low |
| C10 | Freshness model (Q10, FR-015) | Pinned version + explicit `graph.session.refresh`. No deltas. Staleness is the caller's choice. | low |
| C11 | Budgets and overflow (Q11, FR-016) | 256 MiB global, 64 MiB per session. Evict ended sessions first, then `MEMORY_BUDGET_EXCEEDED`. No reduced-scope fallback. | **yes**: size inside the host when embedded (E5) |
| C12 | Shared copies across sessions (Q12, FR-017) | No sharing. Single-flight per `(owner, namespace, scope, version)`. | low |
| C13 | What-if edits (Q13, FR-019) | None. A write naming a session returns `READ_ONLY_SESSION`. | low |
| C14 | Service vs embedded (Q14) | **Both**, from one engine (E1; constitution 3.0.0). | low (the user's own request) |
| C15 | EHDB L0 reuse terms (Q15, R3) | Not linked. ehgraph's own substrate in the EHDB L0 shape. A future pin must equal the host's EHDB pins (E8). | **yes** |
| C16 | Language, licence (Q16, constitution) | Rust (toolchain 1.99.1, edition 2021), Arrow in memory. Apache-2.0. | low (confirm the licence) |
| C17 | Scale / SLO (Q17, SC-003, SC-004, SC-005, Assumptions) | Smoke thresholds only: a 10k-node / 20k-edge load in under 5 s. The planning size is ~10^6 elements per domain on one node. No p95 targets, no per-element byte cap. | **yes**: before any SLO |
| C18 | Instance model (Q18, R4, FR-025) | Stage 0 single primary. `role=replica\|standby` refuses to start. In embedded mode the primary is the host process and the writer is single by declaration: `[embedded] host_replicas = 1` (plus ordinal 0 / leader where the host has them) or the open refuses (E2, **decided 2026-11-06**). | low: decided; a multi-replica host keeps that domain standalone |
| C19 | RPO/RTO; object store (Q19, FR-024, SC-008) | **Implemented (noetl/ehgraph#8):** `backup_prefix` is a directory or `gs://bucket/prefix` (`ObjectStore`: put-if-absent via `ifGenerationMatch=0`, manifest then head, checkpoint last; Workload Identity, or a host token through `OpenOptions`). RPO target = `backup_interval_s + seal_max_age_s` (60 s at the example config). RTO target 15 min per domain at 10^6 elements, not yet measured on a real bucket. Alerts in `deploy/alerts/ehgraph-backup.rules.yaml`. S3 not implemented. | RTO measurement (C17) |
| C20 | Initial domains and names (Q20, FR-022/023) | `ehgraph-network` / `network`, `ehgraph-noetl-cluster` / `noetl-cluster`. | low |
| C21 | Missing edge endpoints (data model) | `INVALID_ARGUMENT`. No placeholder nodes. | low |
| C22 | Nested maps in properties (data model) | Allowed. Timestamps are `{"$timestamp": …}`. | low |
| C23 | Idempotency-key window (data model) | **Implemented (noetl/ehgraph#5):** bounded per namespace by `idempotency_window_s` (default 24 h) on the log clock (max committed `ts_ms`, monotonic) and by `idempotency_max_keys` (default 100,000), oldest first. Eviction happens at the same log position live and on replay, so the window is rebuilt exactly. A retry after the window is a new write (documented in API-and-MCP). Metrics: keys, bytes, evicted. The hash covers namespace, elements and `source`. Compaction (C02) must keep the window's frames or carry the keys. | Window length per domain |
| C24 | MCP layer: hand-rolled or a crate (plan) | A hand-rolled JSON-RPC subset (`2024-11-05`: initialize, ping, tools/list, tools/call). | low |
| C25 | Does `kind: Mcp` lifecycle dispatch pass the resource? (contract) | Yes, as assumed by the shared playbooks: the dispatcher fills `workload.mcp_resource`, and the playbook derives `domain` from the last path segment. Not executed against a NoETL runtime (T016). | **yes**: verify on a real dispatcher |
| C26 | Session lifecycle event sink (contract, FR-020) | `tracing` logs + the in-process registry event list + metrics. No NoETL callback. | medium |
| C27 | Rollback safety of on-disk format (new, E9) | **Implemented 2026-11-06.** Every on-disk JSON file carries `format` (`STORE_FORMAT = 1`) and decodes with `deny_unknown_fields`; a newer format or an unknown field refuses the open (`UNAVAILABLE`, naming the file). Frames refuse unknown versions as before. | low: decided. Rule: any shape change bumps `STORE_FORMAT` |

Embedded-specific detail for C04–C07, C11, C15, C18, C19 and C27 is in E1–E9 below.

## Embedded mode (2026-11-06) — provisional answers, adopted; owner may revisit

The user asked (relayed 2026-11-06) for ehgraph to be usable across projects in **both**
modes: embedded in-process in noetl-server, and standalone. That reverses Q14 and
constitution 2.0.0 Principle I, so the change ships with constitution 3.0.0. That was
adopted on the user's authorization to merge, 2026-11-06. Each item is the default the code
now implements. The "Owner's call" column lists what the owner may still want to revisit.
None of it blocks.

**Seam.** `crates/ehgraph` is the one supported API. `EhGraph` owns the store, the
session registry, grant resolution, the router, and maintenance. `ehgraph-server` adds only
`listen`, the CLI, tracing and signals. noetl-server is Rust on axum 0.8 and tokio 1, the
same stack (checked in `noetl/server` `Cargo.toml`, 2026-11-06), so it can both call
in-process and `nest()` the router.

| # | Forced by | Provisional answer (implemented) | Owner's call |
|---|---|---|---|
| E1 | Q14, constitution I, FR-021 | Both modes, one engine. The hosts' API is `EhGraph`. Inner crates stay internal. | **Ratify 3.0.0**, or reject and keep 2.0.0 (then `crates/ehgraph` is just the server's internal layer). |
| E2 | Q18, FR-025, Instance Model | **Decided by the owner 2026-11-06: an embedded writer is single-writer only.** Three guards: (1) `flock` + process-wide path claim refuse a second handle on one directory; (2) a process-wide claim on `instance_id` *and* `domain` refuses a second embedded primary for either, even on another directory; (3) `EmbeddedConfig::check_single_writer` refuses the open unless the host declares `host_replicas = 1`, and, where present, `host_ordinal = 0` and `leader = true`. The lock is never relied on across pods. noetl-server adds its own refusal on multi-replica evidence (`NOETL_SHARD_COUNT > 1`, NATS-KV coherence, execution affinity, a StatefulSet ordinal > 0). | Decided. Where the host cannot make the declaration, the domain stays standalone. |
| E3 | Q5, FR-008 | In-process calls present a bearer token and get the same per-namespace grants as HTTP. There is deliberately no public "trusted caller" bypass. The host uses its own grant (e.g. an operator `*` grant) for internal calls. | May the host assert a principal for an end user (a trusted path), and should grants come from the NoETL keychain / `check_playbook_access` instead of the TOML? |
| E4 | Q6, FR-009, FR-022/023 | An embedded instance is addressed at the host's mounted route `<host>/ehgraph/<domain>/mcp`. Its `kind: Mcp` resource points there. Deploy/undeploy are the host's own. `/healthz` and `ehgraph_info{mode}` say which mode is serving. **Decided by the owner 2026-11-06:** `noetl-cluster` is embedded in noetl-server (mounted at `/ehgraph/noetl-cluster`); `network` (qnetmon) stays standalone. An embedded domain has no StatefulSet, and its resource carries only read-only verbs (`noetl/ehgraph#10`). | Decided. |
| E5 | Q11, FR-016, constitution IV | The budget defaults are unchanged (256 MiB global, 64 MiB per session). In embedded mode they come out of the host's memory limit. | Budget numbers for the noetl-server pod. |
| E6 | Q19, FR-024 | Scheduled backup runs on the host's runtime via `start_maintenance` (`backup_interval_s`). `ehgraph backup` (the CLI) cannot run while the host holds the lock; use `EhGraph::backup_now` or `POST …/v1/admin/backup` (operator grant). | RPO for an embedded domain. Whether the host's own backup schedule should drive it. |
| E7 | Q7, FR-010/014 | Unchanged: the owner is the string the caller passes. An embedding host *could* close sessions on gateway logout in-process, but nothing wires that. | Should noetl-server bind session owners to gateway sessions and close them on logout? |
| E8 | Q15 | Unchanged: no EHDB link. ⚠ New constraint: if ehgraph ever links `ehdb-l0`/`ehdb-core`, its pin MUST equal noetl-server's (`noetl/server` already says its EHDB pins "move together"), or one binary carries two `ehdb-l0`s. | Part of Q15. |
| E9 | new (lifecycle) | An embedded instance upgrades and **rolls back with its host**. **Implemented 2026-11-06** (owner's call): `crates/ehgraph-store/src/format.rs`. `manifest.json`, `head.json`, `instance.json`, `active.meta.json` and the backup `checkpoint.json` carry `"format": 1`, and every struct read from them (nested part/snapshot entries too) is `deny_unknown_fields`. A newer `format`, or a field this build does not know, refuses the open or restore with a message naming the file; the refused open writes nothing, and restore checks the checkpoint before copying. Files with no marker (the MVP) are format 1. Change-log frames keep their own version byte. Same class of fix as ehdb#439. Tests: `ehgraph-store/tests/format_marker.rs`, `format.rs` unit tests, `ehgraph-backup/tests/roundtrip.rs`. | Decided. Rule for every future change: a new field in a marked file, even an optional one, bumps `STORE_FORMAT`. `Scope` (an API type persisted inside snapshot entries) is covered by the marker rule, not by `deny_unknown_fields`. |

Other changes embedding forced (not spec questions):
- The library installs no runtime, tracing or signal handler. Maintenance requires the
  caller's tokio runtime and errors outside one.
- `shutdown` refuses new calls, drains in-flight ones (with a timeout), and releases the
  lock, so a host can hand the directory over without exiting. Drop without shutdown is
  safe: tasks hold only a `Weak` and are aborted.
- Engine calls do fsync, so routes and `call_async` use the blocking pool. Before this,
  the MVP's handlers ran them on async workers, which is harmless in a dedicated server and
  a host-wide stall when embedded.
- `auth.mode = "disabled"` is refused when embedded unless
  `auth.allow_disabled_embedded = true`, because the routes are on the host's listener.
- Correction: the MVP comment "the same process can lock the file again" was wrong for
  `flock` on macOS (measured: a second open in-process is refused). The in-process claim
  makes the guarantee independent of that.

## Open questions (collected)

Every row has a provisional answer in the Clarification register above (C01–C27).

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
| Q14 | Service vs embedded library. Was **RESOLVED: service** (constitution 2.0.0, 2026-11-03). **REOPENED and re-resolved 2026-11-06**: both modes, from one engine (E1, constitution 3.0.0) | R2, E1, C14 |
| Q15 | EHDB L0 reuse terms and stable API | R3 |
| Q16 | Language (Rust), licence | constitution |
| Q17 | Scale and SLO targets per domain | SC-003, SC-004, Assumptions |
| Q18 | Final instance model: (a) single, (b) primary + replicas/standby, (c) distributed. **Recommended: (a) for the MVP, (b) next, (c) deferred** | R4, FR-025 |
| Q19 | RPO/RTO per domain; object store for backups (GCS, S3-compatible, mounted volume) | FR-024, SC-008 |
| Q20 | Initial domains and instance names (proposed: `noetl-cluster`, `network`) | FR-022, FR-023 |
