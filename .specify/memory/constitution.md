<!--
Sync Impact Report
- Version change: template → 1.0.0 (initial ratification)
- Principles: I. NoETL Ecosystem Component, Consumer-Agnostic; II. The Change Log Is
  the Authority; III. Versioned, Repeatable Reads; IV. Bounded Session Memory;
  V. NoETL-Native Access; VI. Reuse Libraries, Own the State; VII. Test the Failure
  Conditions
- Added sections: Technology & Platform Constraints; Development Workflow; Governance
- Templates reviewed: plan-template.md (Constitution Check + Complexity Tracking) ✅;
  spec-template.md ✅; tasks-template.md ✅
- Follow-ups (deferred, NEEDS CLARIFICATION in specs/001-ehgraph-mvp): licence; language
  confirmation (Rust proposed); service vs embedded-library posture under ai-meta
  `agents/rules/self-sufficiency.md`
-->

# ehgraph (Event Horizon Graph) Constitution

ehgraph is a purpose-built **property-graph database for the NoETL ecosystem**. It
provides a durable change log, versioned snapshots, session-scoped shared-memory graphs,
and access through MCP and HTTP, so NoETL playbooks and agents can use it as a tool.
Its first consumer is qnetmon (network topology), but it is not specific to qnetmon.

Grounding: NoETL platform sources in `noetl/ai-meta` → `repos/noetl`, `repos/docs`,
`repos/cli`, `repos/worker`, `repos/ehdb`. The cited facts are listed in
`specs/001-ehgraph-mvp/research.md`.

## Core Principles

### I. NoETL Ecosystem Component, Consumer-Agnostic

- ehgraph MUST be usable by any NoETL consumer. Consumer-specific node/edge types,
  identity rules and business logic live in the consumer's schema config and playbooks,
  never in ehgraph code.
- Graphs are isolated by **namespace**. One consumer cannot read or write another
  consumer's namespace without an explicit grant.
- Features are justified by a consumer need written in a spec. ehgraph does not add
  general-database features that nobody has asked for.

### II. The Change Log Is the Authority

- Every mutation MUST be appended to a durable, ordered change log, with a
  monotonically increasing `graph_version` per namespace, **before** it is acknowledged.
- Materialized state, indexes, snapshots and in-memory session graphs are **derived**.
  Each MUST be rebuildable from the log plus snapshots. Losing one MUST NOT lose data.
- Writes are idempotent: stable element ids, plus optional client idempotency keys.
  Replaying the same batch MUST NOT change state or the version.

### III. Versioned, Repeatable Reads

- Snapshots are immutable and addressed by `graph_version`.
- Every read result MUST report the `graph_version` it reflects.
- A session reads exactly one version until it explicitly refreshes. A refresh swaps
  atomically, so no reader ever sees a mix of versions.

### IV. Bounded Session Memory

- In-memory graphs are scoped to a **graph session**. There is one shared copy per
  session. It is resident until explicit close or idle expiry, then released.
- Residency MUST be bounded by an idle lease, a maximum lifetime, a per-session cap and a
  global budget.
- An active session MUST NOT be evicted silently. When memory runs out, the response is
  explicit (reject, wait, reduce the scope, or fall back to durable reads).
- Session memory is never the only copy of anything. This is the same rule as the NoETL
  distributed-runtime spec §6 for the shared-memory tier.
- Session graphs are **read-only**. All writes go through the change log.

### V. NoETL-Native Access

- The API MUST be available as MCP tools (`tools/list`, `tools/call`), callable from
  playbooks with `kind: mcp`, and as equivalent HTTP routes. A first-class NoETL tool
  kind may be added later through noetl-tools.
- ehgraph is deployed and operated as a NoETL `kind: Mcp` catalog resource with
  lifecycle playbooks (deploy, undeploy, status, discover). Callers authenticate with
  credentials from the NoETL keychain.
- The surface stays narrow and explicit: fixed read/traversal operations, plus batch
  writes. A query language is added only by amendment.

### VI. Reuse Libraries, Own the State

- ehgraph prefers maintained libraries to hand-rolled subtle algorithms (ai-meta
  `agents/rules/self-sufficiency.md`).
- It reuses EHDB storage pieces **as libraries**: the `DurableSubstrate`, immutable
  parts, frames, the versioned `Manifest`, and the content-addressed object store.
  These are pinned as git dependencies by tag, the same way `noetl/catalog` and
  `noetl/signal-mesh` consume EHDB.
- ehgraph MUST NOT store graph data in an EHDB instance or an EHDB dataset. EHDB's
  program invariant (`ehdb-l0/src/lib.rs`) says business/domain data is never in EHDB.
- ehgraph MUST NOT require an external database or coordination service in order to run.
  Its own state lives on a substrate it controls (a local filesystem or an object store).

### VII. Test the Failure Conditions

- Each guarantee in II–IV MUST have a test that reproduces the condition it protects
  against, not just the happy path. Examples: a crash between append and acknowledgement,
  a restart followed by a rebuild, a concurrent first load, idle expiry, budget overflow,
  and a refresh while readers are active.
- Each mechanism MUST be observable when it fires: metrics and events for loads,
  evictions, expiries and versions. A mechanism that exists but never fires is a defect.

## Technology & Platform Constraints

- Language: **Rust** (proposed). This matches EHDB, the NoETL worker, the CLI and
  `noetl-arrow-cache`. In-memory format: Apache Arrow record batches plus adjacency
  indexes. [NEEDS CLARIFICATION: confirm.]
- Shared memory: when colocated processes attach to a session graph, ehgraph reuses the
  `noetl-arrow-cache` `IpcHint` / POSIX shm conventions. That attach works on the same
  node only; remote readers use the network API.
- Licence: [NEEDS CLARIFICATION: Apache-2.0 to match EHDB?]. The repository has no
  licence file yet.
- Deployment posture: [NEEDS CLARIFICATION: is ehgraph a session-lived service
  (`kind: Mcp`), or an embeddable library that NoETL server/worker hosts, or both? The
  ai-meta self-sufficiency rule says to "push back hard" on new separately-running
  services. The design therefore keeps the core in library crates with a thin server
  crate, so either answer remains possible.]

## Development Workflow

- Spec-kit drives the work: specify, then clarify, then plan, then tasks, then
  implement. Open questions stay as `[NEEDS CLARIFICATION]` until they are answered.
  They are never guessed.
- Each plan MUST pass the Constitution Check. Each deviation is listed in Complexity
  Tracking with the simpler alternative that was rejected.
- Each change updates the ehgraph wiki and the ai-meta central record, per ai-meta
  `agents/rules/change-documentation.md`.

## Governance

- This constitution overrides other practices in this repository.
- Amendments are made by pull request with a Sync Impact Report. Versioning follows
  semver: MAJOR when a principle is removed or redefined, MINOR when a principle or
  section is added or materially expanded, PATCH for clarifications.
- Reviews check compliance with Principles II–IV and VI first, because their violations
  fail silently.

**Version**: 1.0.0 | **Ratified**: 2026-11-03 | **Last Amended**: 2026-11-03
