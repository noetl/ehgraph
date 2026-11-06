<!--
Sync Impact Report — 3.0.0 (2026-11-06, PR noetl/ehgraph#2)
- STATUS: ADOPTED on 2026-11-06, on the user's explicit authorization to finish and merge
  without waiting for ratification. It reverses the 2.0.0 decision recorded below
  ("service vs embedded … resolved by the user, 2026-11-03"). The provisional answers it
  relies on (research.md E1–E9, C01–C27) stay open to the owner's revisit.
- Version change: 2.0.0 → 3.0.0
  - MAJOR because Principle I is redefined again: ehgraph is ONE engine with TWO supported
    deployment modes — embedded in-process in a host (noetl-server) and the standalone
    server. Request: the user, relayed 2026-11-06 ("usable across all projects … BOTH
    deployment modes").
- Modified principles:
  - I. A Standalone Topology Database Server → I. One Topology Engine, Two Deployment Modes
  - IV. Bounded Session Memory: session graphs live in the process that hosts the instance
    (the ehgraph server, or the embedding host)
  - V. NoETL-Native Access: an embedded instance is also reachable in-process; its
    `kind: Mcp` resource points at the host's mounted routes
- Accepted Exceptions: Exception 1 and 2 now apply to standalone mode only. Embedded mode in
  noetl-server deploys nothing alongside NoETL, so it needs neither.
- Instance Model: single primary per instance is unchanged, and now also binds the host —
  only one host process may embed a given instance.
- Templates reviewed: plan-template.md ✅, spec-template.md ✅, tasks-template.md ✅
- Follow-ups: specs/001-ehgraph-mvp/research.md "Clarification register" (C01–C27) and
  "Embedded mode (2026-11-06)" (E1–E9).

Sync Impact Report — 2.0.0
- Version change: 1.0.0 → 2.0.0
  - MAJOR because Principle I is redefined: ehgraph is a standalone topology database
    server, not a component that could be embedded. The deployment posture is resolved
    as SERVICE.
- Modified principles:
  - I. NoETL Ecosystem Component → I. A Standalone Topology Database Server
  - IV. Bounded Session Memory: session graphs live in the server process
  - V. NoETL-Native Access: one `kind: Mcp` resource per instance
  - VI. Reuse Libraries, Own the State: library crates are internal structure only
- Added sections:
  - Accepted Exceptions: self-sufficiency rule and ephemeral principle
  - Instance model: independent single-primary instances, one per topology domain
- Removed: the "service vs embedded" open question (resolved by the user, 2026-11-03)
- Templates reviewed: plan-template.md ✅, spec-template.md ✅, tasks-template.md ✅
- Follow-ups (NEEDS CLARIFICATION in specs/001-ehgraph-mvp):
  - licence; Rust confirmation
  - final instance-model choice (research R4; recommendation stated there)
- Earlier: template → 1.0.0 (initial ratification, 2026-11-03)
-->

# ehgraph (Event Horizon Graph) Constitution

ehgraph is a **topology database**: a standalone database server for topology graphs.
Example domains:

- NoETL services and cluster topology
- network topology
- other topologies NoETL applications need

It stores property graphs with:

- a durable change log
- versioned snapshots
- session-scoped shared-memory graphs
- access through MCP and HTTP, so NoETL playbooks and agents can use it as a tool

It runs in one of two modes, from the same engine:

- **Embedded**: in-process inside a host — first noetl-server — as a library, with no
  separate process and no network hop for the host's own calls.
- **Standalone**: operated like PostgreSQL, a database server you deploy and connect to.

Either way it is not an application microservice.

Grounding: NoETL platform sources in `noetl/ai-meta` → `repos/noetl`, `repos/docs`,
`repos/cli`, `repos/worker`, `repos/ehdb`. The cited facts are in
`specs/001-ehgraph-mvp/research.md`.

## Core Principles

### I. One Topology Engine, Two Deployment Modes

- **The product is the engine.** The `ehgraph` crate is the supported embedding API. The
  `ehgraph` server binary and its container image wrap that same crate.
  - **Embedded:** a host opens an instance in-process (`EhGraph::open_embedded`). It calls
    tools directly, and it may mount the instance's MCP/HTTP routes on its own listener.
  - **Standalone:** the server binary serves one instance on its own listener and volume.
  - The two modes MUST share one engine. They differ only in transport and lifecycle:
    the same log format, the same tools, the same grants, the same guarantees (II–IV).
    A guarantee that holds in only one mode is a defect.
  - The crates behind `ehgraph` (`-core`, `-store`, `-memory`, `-api`, `-backup`) stay
    internal structure. Hosts depend on `ehgraph` only.
- **Consumer-agnostic.** ehgraph MUST be usable by any consumer and any topology domain.
  Domain-specific node/edge types, identity rules and business logic live in the
  consumer's schema config and playbooks. They are never in ehgraph code.
- **Domains and namespaces.**
  - A **topology domain** (e.g. `noetl-cluster`, `network`) is served by its own
    ehgraph **instance** (see Instance Model).
  - Inside an instance, graphs are isolated by **namespace**. A consumer cannot read or
    write another namespace without an explicit grant.
- **Need-driven features.** Features are justified by a consumer need written in a spec.
  ehgraph does not add general-database features nobody asked for.

### II. The Change Log Is the Authority

- Every mutation MUST be appended to a durable, ordered change log, with a
  monotonically increasing `graph_version` per namespace, **before** it is acknowledged.
- Materialized state, indexes, snapshots, session graphs and any future replica are
  **derived**. Each MUST be rebuildable from the log plus snapshots.
- Writes are idempotent (stable element ids plus optional idempotency keys). Replaying a
  batch MUST NOT change state or version.

### III. Versioned, Repeatable Reads

- Snapshots are immutable and addressed by `graph_version`.
- Every read result MUST report the `graph_version` it reflects, plus the instance that
  served it.
- A session reads exactly one version until it explicitly refreshes. A refresh swaps
  atomically.

### IV. Bounded Session Memory

- Session graphs are held in memory by the process that hosts the instance: the ehgraph
  server, or the embedding host. In embedded mode the budget is part of the host's
  memory, and the host MUST size it as such.
  - There is one shared copy per session.
  - It stays resident until explicit close or idle expiry, then it is released.
- Residency MUST be bounded by an idle lease, a maximum lifetime, a per-session cap and a
  global budget per instance.
- An active session MUST NOT be evicted silently. Overflow MUST produce an explicit
  response.
- Session memory is never the only copy of anything. Session graphs are **read-only**,
  and all writes go through the change log.

### V. NoETL-Native Access

- The API MUST be exposed as MCP tools (`tools/list`, `tools/call`), callable from
  playbooks with `kind: mcp`, and as equivalent HTTP routes.
- **One instance, one catalog resource.**
  - Each ehgraph instance is registered as its own NoETL `kind: Mcp` catalog resource,
    with its own address and lifecycle playbooks (deploy, undeploy, status, discover).
    For an embedded instance, the address is the host's mounted route
    (`<host>/ehgraph/<domain>/mcp`), and deploy/undeploy are the host's.
  - Embedded in-process calls are not an exception to authorization. They present a
    bearer credential and are checked against the same per-namespace grants.
  - Consumers choose a topology domain by choosing the resource.
- Callers authenticate with credentials from the NoETL keychain.
- The surface stays narrow: fixed read/traversal operations plus batch writes. A query
  language is added only by amendment.

### VI. Reuse Libraries, Own the State

- ehgraph prefers maintained libraries to hand-rolled subtle algorithms.
- It reuses EHDB storage pieces **as libraries**: `DurableSubstrate`, immutable parts,
  frames, the versioned `Manifest`, and the content-addressed object store. These are
  pinned as git dependencies by tag, the same way `noetl/catalog` and `noetl/signal-mesh`
  consume EHDB.
  - Later replication MAY reuse EHDB's change-feed, election and fencing designs or
    crates. See research R4.
- ehgraph MUST NOT store graph data in an EHDB instance or an EHDB dataset. EHDB's
  program invariant says business/domain data is never in EHDB.
- ehgraph MUST NOT depend on another external database or a coordination service to run
  an instance. Its state lives on a substrate it controls: a local volume, with backup to
  an object store.

### VII. Test the Failure Conditions

- Each guarantee in II–IV MUST have a test that reproduces the condition it protects
  against. Examples:
  - a crash between append and acknowledgement
  - a rebuild after a restart
  - restore from backup
  - a concurrent first load
  - idle expiry
  - budget overflow
  - a refresh while reads are active
- Each mechanism MUST be observable when it fires (metrics and events). A mechanism that
  never fires is a defect.

## Instance Model

- **MVP:** multiple independent instances, one per purpose or topology domain.
  - Each instance is a **single primary**: one process, one writer. In embedded mode that
    process is the host. A host MUST NOT run more than one process embedding the same
    instance (for example, a multi-replica noetl-server Deployment where every pod opens
    it): the data-directory lock catches a shared volume, but not two pods on two volumes.
  - Each has its own durable change log and snapshots, and its own backup and restore
    through an object store.
  - Instances share nothing and know nothing about each other.
- **Next stage (planned, not MVP):** a primary plus read replicas or a warm standby, fed
  from the change log, with single-writer election and fencing. EHDB's designs and
  crates are the reuse candidates.
- **Deferred:** sharding and multi-writer, for two reasons:
  - Traversals across partitions destroy locality.
  - Topology graphs for one domain fit in a single node's memory.
- The final choice stays open (research R4; provisional C18). Any move beyond a single
  primary requires an amendment and MUST preserve Principles II and III: one ordered log
  per namespace, and no split-brain.

## Accepted Exceptions

These are deliberate deviations from ai-meta and NoETL platform rules. They are accepted
by the user (2026-11-03) and recorded here and in each plan's Complexity Tracking.

**Scope (3.0.0):** both exceptions apply to **standalone mode only**. Embedded in
noetl-server, ehgraph deploys nothing alongside NoETL and runs no persistent MCP server of
its own, so it needs neither. Exception 1's third argument ("embedding it … would couple
topology storage to NoETL's own release and scaling") remains true of embedded mode. That
is the trade the owner accepts by choosing it per instance.

1. **ai-meta `agents/rules/self-sufficiency.md`.** The rule says "nothing to deploy
   alongside NoETL", and to "push back hard" on a new separately-running service.
   ehgraph is a separately-running database server. Justification:
   - **Persistent state.** ehgraph is a database, like PostgreSQL. It is not an
     application microservice. Its durable state must outlive every session, playbook
     execution and worker.
   - **Shared session memory.** Shared-memory session graphs must be held by a
     long-lived process that every reader in the session can reach. NoETL workers are
     stateless, and the NoETL IPC cache is same-node with 60 s leases.
   - **Many consumers and domains.** Embedding it in NoETL server or worker would couple
     topology storage to NoETL's own release and scaling. It would also prevent separate
     instances per domain.

   Conditions: no external DB or coordination dependency per instance (Principle VI);
   deployed and operated through NoETL `kind: Mcp` lifecycle playbooks.
2. **NoETL ephemeral principle** (`docs/docs/architecture/ephemeral_blueprints.md`:
   "No persistent MCP server instances"). Each ehgraph instance is a persistent MCP
   server. Justification: the same as above. The principle governs workloads and tool
   sessions, and ehgraph is storage.

   Conditions:
   - Session graphs remain bounded and derived (Principle IV).
   - Playbooks and workers hold no ehgraph state.
   - The lifecycle is owned by catalog playbooks.

## Technology & Platform Constraints

- Language: **Rust** (proposed), matching EHDB, the NoETL worker, the CLI and
  `noetl-arrow-cache`. In-memory format: Apache Arrow record batches plus adjacency
  indexes. [PROVISIONAL C16, research.md register: confirm.]
- Deployment, standalone: container image. Kubernetes `StatefulSet` with `replicas: 1`
  per instance, plus a persistent volume. A local process is also supported for laptops.
- Deployment, embedded: the `ehgraph` crate pinned by the host (git tag), with the data
  directory on a volume the host's single embedding process owns.
- Same-node attach to session graphs in shm (the `noetl-arrow-cache` `IpcHint` format) is
  optional. Remote readers always use the network API.
- Licence: [PROVISIONAL C16, research.md register: Apache-2.0 to match EHDB?]

## Development Workflow

- Spec-kit drives the work: specify, then clarify, then plan, then tasks, then
  implement. Open questions stay `[PROVISIONAL C18, research.md register]` until answered.
- Each plan MUST pass the Constitution Check, with deviations listed in Complexity
  Tracking.
- Each change updates the ehgraph wiki and the ai-meta central record.

## Governance

- This constitution overrides other practices in this repository.
- Amendments are made by pull request with a Sync Impact Report. Versioning follows
  semver:
  - MAJOR: a principle is removed or redefined.
  - MINOR: a principle or section is added, or materially expanded.
  - PATCH: clarifications.
- Reviews check Principles II–IV, VI and the Instance Model first, because their
  violations fail silently.

**Version**: 3.0.0 | **Ratified**: 2026-11-03 (2.0.0); 3.0.0 adopted 2026-11-06 on user authorization, provisional items revisitable | **Last Amended**: 2026-11-06
