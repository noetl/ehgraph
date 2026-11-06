<!--
Sync Impact Report
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

It is operated like PostgreSQL: a database server you deploy and connect to. It is not an
application microservice, and it is not a library linked into a consumer.

Grounding: NoETL platform sources in `noetl/ai-meta` → `repos/noetl`, `repos/docs`,
`repos/cli`, `repos/worker`, `repos/ehdb`. The cited facts are in
`specs/001-ehgraph-mvp/research.md`.

## Core Principles

### I. A Standalone Topology Database Server

- **The product is the server.** The ehgraph product is the `ehgraph` database server
  binary and its container image.
  - Library crates are internal structure. They are not a supported way to embed
    ehgraph inside another process.
  - Consumers reach ehgraph only over its network API (Principle V).
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

- Session graphs are held in memory by the ehgraph server process that serves the
  session.
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
  - Each instance is a **single primary**: one process, one writer.
  - Each has its own durable change log and snapshots, and its own backup and restore
    through an object store.
  - Instances share nothing and know nothing about each other.
- **Next stage (planned, not MVP):** a primary plus read replicas or a warm standby, fed
  from the change log, with single-writer election and fencing. EHDB's designs and
  crates are the reuse candidates.
- **Deferred:** sharding and multi-writer, for two reasons:
  - Traversals across partitions destroy locality.
  - Topology graphs for one domain fit in a single node's memory.
- The final choice stays open (research R4, NEEDS CLARIFICATION). Any move beyond a single
  primary requires an amendment and MUST preserve Principles II and III: one ordered log
  per namespace, and no split-brain.

## Accepted Exceptions

These are deliberate deviations from ai-meta and NoETL platform rules. They are accepted
by the user (2026-11-03) and recorded here and in each plan's Complexity Tracking.

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
  indexes. [NEEDS CLARIFICATION: confirm.]
- Deployment: container image. Kubernetes `StatefulSet` with `replicas: 1` per instance,
  plus a persistent volume. A local process is also supported for laptops.
- Same-node attach to session graphs in shm (the `noetl-arrow-cache` `IpcHint` format) is
  optional. Remote readers always use the network API.
- Licence: [NEEDS CLARIFICATION: Apache-2.0 to match EHDB?]

## Development Workflow

- Spec-kit drives the work: specify, then clarify, then plan, then tasks, then
  implement. Open questions stay `[NEEDS CLARIFICATION]` until answered.
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

**Version**: 2.0.0 | **Ratified**: 2026-11-03 | **Last Amended**: 2026-11-03
