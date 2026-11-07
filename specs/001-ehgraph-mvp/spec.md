# Feature Specification: ehgraph MVP — a topology database server: durable property graphs, versioned snapshots and session-scoped shared-memory graphs

**Feature Branch**: `001-ehgraph-mvp` (tracked on git branch `spec/initial`)

**Created**: 2026-11-03

**Status**: Draft

**Input**: User decision (2026-11-03, while specifying qnetmon): "design and develop graph
database to store network topology graph and be able to load it to the shared memory when
user session requests to read it and keep it in shared memory until the session ends."
The user chose to **build** it because "we need graph database anyway". They named it
**ehgraph** (Event Horizon Graph) and created <https://github.com/noetl/ehgraph>.

**Product decision** (user, 2026-11-03): "let's have ehgraph as database that used for
topology - it can be noetl services / cluster topology or network topology but i think it
should be a separate database - not a library. single instance - so we can have many
instances if needed for different purposes. not sure though what design is better single
instance or distrbuted multiple instance or replicated." What this spec takes from that:

- ehgraph is a **topology database server**, not a library (constitution 2.0.0, Principle I).
- Each instance serves one **topology domain**. Examples:
  - `noetl-cluster`: NoETL services and cluster topology
  - `network`: network topology, used by qnetmon
- The MVP instance is a single primary. Replicas are the planned next stage, and sharding
  is deferred. The comparison and the open final choice are in research R4 and Q18.

**Origin**: this spec carries over the graph-database MVP first written in qnetmon
(`shastaranch/qnetmon`, `specs/001-noetl-network-monitor`, branch `spec/noetl-core`):

| ehgraph | qnetmon origin |
|---|---|
| FR-001–FR-009 | FR-037–FR-045 (graph database) |
| FR-010–FR-020 | FR-020–FR-030 (session graph) |
| research R1–R3 | research R7–R9 |
| SC-001–SC-006 | SC-007–SC-010 + new |

Everything is rewritten so it stands on its own. qnetmon keeps only its consumer-side
requirements and references this spec.

**Starting point**: the repository holds one commit (`0b5c95b`, `.gitignore` only). The
wiki holds a default Home page (`2b932f2`).

## User Scenarios & Testing *(mandatory)*

"Consumer" means a NoETL playbook, an agent tool or an MCP client acting for a NoETL
application. Expected first consumers:

- a NoETL services/cluster topology (domain `noetl-cluster`)
- qnetmon's network topology (domain `network`)

"Instance" means one running ehgraph database server, which serves one topology domain.

### User Story 1 - Write a graph durably and idempotently (Priority: P1)

A consumer playbook upserts batches of nodes and edges into its namespace and deactivates
elements it no longer sees. Each write is durable when acknowledged, returns the new
`graph_version`, and can be replayed safely.

**Why this priority**: nothing else exists without durable, idempotent writes.

**Independent Test**: apply a fixture batch twice, then kill the process between append
and acknowledgement and restart. The state and version are the same as one clean apply,
and the state rebuilds from the log.

**Acceptance Scenarios**:

1. **Given** an empty namespace, **When** a batch of N nodes and M edges is upserted,
   **Then** it is acknowledged with `graph_version = v1` and reads return those elements.
2. **Given** the same batch (same ids or idempotency key) again, **When** it is applied,
   **Then** state and `graph_version` are unchanged.
3. **Given** a crash after the append but before the acknowledgement, **When** the
   service restarts, **Then** the batch is either fully present at a single version or
   absent. It is never partially present.
4. **Given** elements not seen since time T, **When** the consumer deactivates by
   `{source, not_seen_since}`, **Then** they are marked inactive (soft history), not
   deleted.

### User Story 2 - Read a consistent version through MCP and HTTP (Priority: P1)

A consumer reads nodes, neighbors, k-hop subgraphs and paths through MCP tools (from a
playbook with `kind: mcp`) or HTTP. Every result says which `graph_version` it reflects,
and old versions stay readable through snapshots.

**Why this priority**: consumers and agents need traversal to be useful. Version tags
make the answers auditable.

**Independent Test**: from a NoETL playbook run locally, call each read tool against a
fixture namespace and compare the results with the expected JSON, including the
`graph_version`.

**Acceptance Scenarios**:

1. **Given** a namespace at version v, **When** any read tool is called, **Then** the
   result carries `graph_version = v`.
2. **Given** a snapshot at v1 and later writes up to v2, **When** a read asks for v1,
   **Then** it returns v1 state exactly.
3. **Given** a caller without read access to the namespace, **When** it calls a tool,
   **Then** the call fails with `UNAUTHORIZED`.

### User Story 3 - Session-scoped shared-memory graph (Priority: P1)

When a user session first reads the graph, the requested scope is loaded **once** from
durable storage into memory. Every reader in that session uses that one copy. The copy
stays resident until the session ends or goes idle, and is then released.

**Why this priority**: this is the requirement that motivated building ehgraph.
Interactive and multi-turn agent analysis traverses the same graph repeatedly, and
re-reading durable storage on every tool call is slow and inconsistent while writes
continue.

**Independent Test**: open a session over the fixture namespace, then issue N reads from
two callers. Check that there was exactly one durable load and the same version on every
read. Check that memory is released after an explicit close, and, in a second run,
after the idle timeout.

**Acceptance Scenarios**:

1. **Given** no resident graph for session S, **When** S issues its first read, **Then**
   the scope is loaded once, tagged with its version, and the read is answered from
   memory.
2. **Given** S is resident, **When** another reader in S reads, **Then** it gets the same
   copy and version, with no new load.
3. **Given** writes after S loaded, **When** S reads again, **Then** it still sees its
   pinned version until an explicit refresh (FR-015).
4. **Given** S is closed or idle-expired, **When** a reader uses S's id, **Then** the
   read fails with `SESSION_EXPIRED`. It never returns stale data.
5. **Given** the memory budget is exhausted, **When** a new session asks to load,
   **Then** the response is explicit (FR-016). No active session is silently evicted.

### User Story 4 - Operate ehgraph instances as NoETL catalog resources (Priority: P2)

An operator deploys one ehgraph instance per topology domain, for example
`ehgraph-noetl-cluster` and `ehgraph-network`.

- Each instance is registered as its own `kind: Mcp` catalog resource and runs the shared
  lifecycle playbooks (deploy, status, discover, backup, restore, undeploy), parameterized
  by instance.
- Health and metrics show the instance identity, sessions, resident bytes, load latency
  and versions.

**Why this priority**: this is needed for shared environments. For a laptop, a local
process is enough (FR-009).

**Independent Test**: on local kind, deploy through the lifecycle playbook. `status`
reports healthy, `discover` lists the tools, and `undeploy` removes ehgraph cleanly.

**Acceptance Scenarios**:

1. **Given** the resource is registered, **When** `deploy` then `status` run, **Then**
   status is healthy and `discover` lists the tools in contracts/graphdb.md.
2. **Given** a running instance, **When** sessions open and close, **Then** the metrics
   for sessions and resident bytes move accordingly.
3. **Given** two instances for different domains, **When** one is written, undeployed or
   restored, **Then** the other's data, versions and sessions are unaffected.
4. **Given** an instance with a backup in the object store, **When** its volume is
   discarded and `restore` runs, **Then** every namespace comes back at the backed-up
   `graph_version`, and later writes continue from that version.

### Edge Cases

- Session never closed (client crash, lost connection): the idle lease (FR-014) releases
  it. A crashed process loses only derived state, and the next open reloads it.
- Two concurrent first reads for the same `(owner, scope)`: there is one load, both get
  the same session, and neither sees a half-loaded graph.
- Scope larger than the per-session cap: the load is refused or reduced, with an
  explicit result.
- Reader on another node than the resident copy: it is served over MCP/HTTP and never by
  attaching to another node's shared memory. The NoETL IPC cache refuses cross-node
  attach by design.
- Refresh while reads are in flight: in-flight reads finish on the old version. New
  reads see the new version after the atomic swap.
- Deactivate followed by re-upsert of the same id: the element becomes active again, and
  the history shows both transitions.
- Very high write rate during a session load: the load reads one consistent snapshot plus
  the log tail up to a fixed version. It never reads a moving target.

## Requirements *(mandatory)*

### Functional Requirements

**Graph database**

- **FR-001**: **Property-graph model**: nodes and directed edges, each with a stable id,
  a type label, a typed property map, soft-history fields (`first_seen`, `last_seen`,
  `active`) and provenance (`source`, `execution_id`). Multiple namespaces exist per
  deployment, isolated from each other. [PROVISIONAL C01, research.md register: is the schema declared per
  namespace (types and required properties, validated on write), or schemaless in the
  MVP?]
- **FR-002**: **Durable storage**: every mutation MUST be appended to a durable, ordered
  change log with a monotonically increasing `graph_version` per namespace before it is
  acknowledged. Materialized state MUST be rebuildable from the log plus snapshots. The
  storage pieces SHOULD reuse EHDB L0 crates as libraries (research.md R3). Graph data
  MUST NOT be stored in EHDB.
- **FR-003**: **Versioned snapshots**: immutable snapshots of a namespace (or a scope) at
  a `graph_version`, listed in a versioned manifest and readable by version, with a
  retention policy. [PROVISIONAL C02, research.md register: snapshot cadence (every N changes,
  time-based, or on demand) and retention.]
- **FR-004**: **Session-scoped shared-memory loading**: provided by FR-010–FR-020.
- **FR-005**: **Query and traversal API** (read): get node or edge by id; filter by type,
  origin and properties; neighbors by edge type and direction; k-hop subgraph; shortest
  path; snapshot and version info. Every result carries its `graph_version`. Results are
  bounded by limits or cursors. [PROVISIONAL C03, research.md register: fixed operations only for the MVP,
  or also an openCypher/GQL subset?]
- **FR-006**: **NoETL exposure**: the API MUST be exposed as MCP tools (`tools/list`,
  `tools/call`) callable with `kind: mcp`, and over HTTP. Agent tools bind to read-only
  sub-playbooks that call it. **Each instance** is registered as its own `kind: Mcp`
  catalog resource with lifecycle playbooks (FR-023). [PROVISIONAL C04, research.md register: should it also become a first-class
  NoETL tool kind (e.g. `kind: graph`) in noetl-tools?]
- **FR-007**: **Writes**: batch upsert and deactivate of nodes and edges, idempotent by
  stable id and an optional client idempotency key, returning the new `graph_version`.
  Writes are never accepted on session handles (FR-019).
- **FR-008**: **Auth and tenancy**: callers authenticate with a credential from the
  NoETL keychain. Access is granted per namespace, separately for read and write.
  [PROVISIONAL C05, research.md register: token type, and whether to integrate with NoETL server auth
  (`check_playbook_access`).]
- **FR-009**: **Operability**: health/status for the `kind: Mcp` `status` verb; metrics
  for sessions, resident bytes, load latency, evictions and versions. [PROVISIONAL C06, research.md register: deployment targets for the MVP — Kubernetes only, or also a local
  process for laptop runs (`noetl run --runtime local`)?]

**Topology database server and instances**

- **FR-021**: **Two deployment modes, one engine** (amended 2026-11-06 with constitution
  3.0.0; it was "standalone only"):
  - **Standalone:** one binary and container image, with its own durable volume.
    Consumers reach it over the MCP/HTTP API.
  - **Embedded:** the `ehgraph` crate opened in-process by a host (noetl-server). The host
    calls tools directly, with no network hop, and MAY mount the same MCP/HTTP routes on
    its own listener.
  - Both modes MUST enforce FR-002/FR-007/FR-008/FR-025 and FR-010–FR-020 identically.
    Tests MUST cover each guarantee in the embedded path, not only the standalone one.
- **FR-022**: **Instance and domain identity**: each instance MUST be configured with:
  - a stable `instance_id`
  - one `domain` label (e.g. `noetl-cluster`, `network`)
  - its own data directory and object-store prefix

  Every response and every metric carries `instance_id` and `domain`. Instances share no
  state, and a write, restore or undeploy on one MUST NOT affect another. Namespaces are
  unique within an instance.
- **FR-023**: **Instance addressing**: each instance MUST be registered as one `kind: Mcp`
  resource, named `ehgraph-<domain>` with path `mcp/ehgraph/<domain>`.
  - The resource holds the instance's own URL and `deployment.namespace`.
  - Lifecycle playbooks are shared and take the instance as a parameter.
  - Consumers select an instance only by its catalog resource. ehgraph has no
    cross-instance routing.
- **FR-024**: **Backup and restore**: an instance MUST ship its sealed change-log parts,
  snapshots and manifest to an object store under its own prefix.
  - It MUST be able to restore every namespace from that prefix into an empty volume, at
  the last backed-up `graph_version`.
  - Backup lag (the RPO) MUST be exposed as a metric. [PROVISIONAL C19, research.md register: target RPO and
  RTO per domain, and the object store (GCS, S3-compatible, or a mounted volume for local
  runs).]
- **FR-025**: **Single writer**: exactly one process accepts writes for an instance.
  - The MVP enforces this with one replica plus a data-directory lock, and a second
    process on the same data directory MUST refuse to start.
  - In embedded mode the lock is held by the host process, and a second handle on the same
    directory in the SAME process MUST also be refused, as MUST a second embedded primary
    for the same `instance_id` or `domain` on another directory.
  - An embedded writer MUST refuse to start unless the host declares a single writer
    (`[embedded] host_replicas = 1`; `host_ordinal = 0` and `leader = true` where the
    host has them). The lock MUST NOT be relied on across pods. A domain whose host cannot
    make the declaration runs standalone (research E2, decided 2026-11-06).
- **FR-026**: **On-disk format marker**: every JSON file in a data directory or backup
  carries `format`. A reader MUST refuse (not ignore) a newer format or an unknown field,
  name the file, and write nothing. Any shape change bumps the format (research E9).
  Each file is written at the lowest format that describes it (format 2 = a compacted
  manifest, C02).
  - Read replicas or a standby are a later stage (research R4). They MUST NOT be enabled
    until election plus store-side fencing are in place.

**Session-scoped shared-memory graph**

- **FR-010**: A **graph session** MUST be opened on a user session's first read (or
  explicitly), identified by a `graph_session_id`, and bound to an owner reference.
  [PROVISIONAL C07, research.md register: what is the owner/"user session"? Options: a NoETL gateway
  session token, an MCP client connection, a `kind: agent` conversation, or an explicit
  session opened by a playbook. NoETL has gateway sessions with `expires_at` and logout,
  but no session-end hook that triggers a playbook (research.md R2). So ehgraph needs
  its own open, close and idle handling.]
- **FR-011**: On open, the requested scope MUST be loaded from durable storage into
  memory once, tagged with the `graph_version` it reflects. The scope is one of: the
  full namespace; a subgraph (by property, such as a site, or the k-hop neighbourhood of
  seed nodes); or a named snapshot. [PROVISIONAL C08, research.md register: is the full namespace alone
  acceptable for the MVP?]
- **FR-012**: All readers in a session MUST read the single in-memory copy. There are no
  per-reader copies.
- **FR-013**: The in-memory graph MUST stay resident until the session ends, and MUST
  then be released (memory freed, registry entry closed).
- **FR-014**: A session MUST end on (a) explicit close, (b) the owner session ending,
  where that is observable, or (c) idle timeout. Every read renews the idle lease.
  [PROVISIONAL C09, research.md register: idle timeout and maximum lifetime values. What should a caller
  see after expiry: an error, or an automatic reopen?]
- **FR-015**: Every in-memory graph MUST carry a version, and every read MUST report it.
  The default is a **pinned** version. An explicit `refresh` MUST reload (or apply
  deltas up to) a newer version atomically. [PROVISIONAL C10, research.md register: is
  pinned-with-explicit-refresh enough, or is periodic or near-live delta application
  required? What is the maximum acceptable staleness?]
- **FR-016**: Memory limits: a global budget and a per-session cap MUST be configured.
  When a load would exceed them, ehgraph first evicts only ended or expired sessions,
  then responds explicitly (wait, reject with `MEMORY_BUDGET_EXCEEDED`, offer a reduced
  scope, or fall back to durable reads). It never evicts an active session silently.
  [PROVISIONAL C11, research.md register: budget sizes and which overflow response.]
- **FR-017**: Concurrent sessions MUST be isolated in lifecycle. [PROVISIONAL C12, research.md register:
  may sessions with the same `(namespace, scope, version)` share one reference-counted
  copy?]
- **FR-018**: Access is through FR-006. Readers on other nodes use the network API, not
  cross-node shared-memory attach. Without a session, the same read tools serve from the
  durable store. Same-node attach to Arrow batches in POSIX shm (`IpcHint` format) is
  optional.
- **FR-019**: The in-memory graph MUST be read-only. Writes go to the change log, and
  sessions see them only after a refresh. [PROVISIONAL C13, research.md register: is session-local,
  never-persisted "what-if" editing needed, such as simulating a failed link?]
- **FR-020**: Session lifecycle events MUST be emitted with session id, owner, scope,
  version and bytes, so residency is auditable and the state can be rebuilt at the
  recorded version. The lifecycle events are: open, load, refresh, close, idle-expire,
  evict, and load failure. When they are invoked through playbooks, they also appear as
  NoETL executions.

### Key Entities

- **Instance**: one running ehgraph server. Attributes: `instance_id`, `domain`, data
  directory, object-store prefix, role (`primary`; later `replica`/`standby`), and
  budgets.
- **Namespace**: within an instance, an isolated graph with its own `graph_version` sequence, schema config
  (FR-001) and access grants.
- **Node / Edge**: stable id, type, props, soft history, provenance, and the versions in
  which it was created and updated.
- **ChangeRecord**: `graph_version`, op, payload, idempotency key, execution id and
  timestamp.
- **Snapshot**: an immutable image of a namespace or scope at a `graph_version`, listed in
  the manifest.
- **GraphSession**: id, owner, scope, pinned version, state, timestamps, idle and maximum
  lifetime, resident bytes, and the serving instance.

## Success Criteria *(mandatory)*

- **SC-001**: Replaying any acknowledged batch changes neither state nor version. The
  crash-between-append-and-acknowledgement test never leaves a partial batch.
- **SC-002**: After deleting all derived state, a rebuild from the log plus snapshots
  reproduces identical query results at the same version.
- **SC-003**: Session load latency: the first read completes within [PROVISIONAL C17, research.md register: target, e.g. p95 < X s for N nodes / M edges]. Later reads (a k-hop
  neighbour query) complete within [PROVISIONAL C17, research.md register: e.g. p95 < Y ms].
- **SC-004**: Memory footprint: resident bytes per session for the reference graph are at
  most [PROVISIONAL C17, research.md register: bytes per node/edge, or a cap]. The global resident total
  never exceeds the budget in the load test.
- **SC-005**: Durable storage is read exactly once per session load or refresh. 100% of
  reads in a session report the same version until a refresh.
- **SC-006**: After close or idle expiry, memory is released within [PROVISIONAL C09, research.md register: grace period], and 100% of later reads with that id return
  `SESSION_EXPIRED`.
- **SC-007**: Every tool in contracts/graphdb.md is exercised by a NoETL playbook
  contract test that passes on the local runtime.
- **SC-008**: Restoring an instance from its object-store backup into an empty volume
  reproduces identical query results at the backed-up version, within [PROVISIONAL C19, research.md register: RTO]. Backup lag stays under [PROVISIONAL C19, research.md register: RPO].
- **SC-009**: Two instances (different domains) run side by side on one cluster. Writing
  to, restoring or undeploying one leaves the other's versions and results unchanged.

## Assumptions

- "Shared memory" means memory shared by all readers of one session, hosted by one
  ehgraph process. The NoETL Arrow IPC cache is reused only for optional same-node
  attach. It is not assumed to be a session store (research.md R2).
- The NoETL runtime (server and workers, or the CLI local runtime) is available to
  consumers. ehgraph does not depend on NoETL internals at runtime, beyond keychain
  credentials and the catalog.
- The MVP is a **single primary per instance**, with one instance per topology domain.
  Read replicas and warm standby are the next stage. Sharding, multi-writer and
  multi-region are deferred (research R4, Q18).
- Out of MVP: a general query language, multi-request write transactions, and graph
  algorithms beyond traversal and shortest path.
- Topology graphs per domain are assumed to fit in one node's memory: up to about 10^6
  nodes and edges. Examples: a NoETL cluster's services, workers, pods and dependencies,
  or a small/medium network.
  [PROVISIONAL C17, research.md register: node/edge counts and write rate per domain.]
