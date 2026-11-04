# Tasks: ehgraph MVP

**Input**: `specs/001-ehgraph-mvp/` (spec, plan, research, data-model, contracts/graphdb.md)

Origin: these tasks carry over qnetmon T033–T055 (Phase 2A and US5).

Format: `[ID] [P?] [Story] Description`. `[P]` marks tasks that can run in parallel
(different files).

## Phase 0: Clarify

- [x] T000 Q14 resolved (user, 2026-11-03): SERVICE. ehgraph is a standalone topology
  database server, with many single-instance deployments per domain. Constitution 2.0.0
  records the accepted exceptions.
- [ ] T001 Resolve the open questions in research.md. First:
  - Q18: instance model. Recommended: single primary for the MVP, replicas next, sharding
    deferred (R4).
  - Q19: RPO/RTO and the backup object store.
  - Q20: initial domains and instance names.
  - Q15: EHDB L0 reuse terms, with EHDB maintainers.
  - Q16: Rust, licence.
  - Q7: session owner.
  - Q11: budgets.
  - Q17: scale/SLO targets per domain for SC-003/SC-004.
- [ ] T002 Update the constitution and plan with the answers. Remove the resolved
  `[NEEDS CLARIFICATION]` markers.

## Phase 1: Setup

- [ ] T003 Cargo workspace with crates per plan.md. Pin `ehdb-l0` (and `ehdb-storage`) to
  one ehdb tag, with a comment that the pins move together. Add LICENSE (T001) and
  README.
- [ ] T004 [P] CI: `cargo fmt --check`, `clippy -D warnings`, `cargo test`.
- [ ] T005 [P] Fixture graphs: `tests/fixtures/small` (hand-written, with expected query
  results) and `tests/fixtures/scale` (generated, sized from T001).

## Phase 2: Foundational — durable store (blocks all stories)

- [ ] T006 Spike: EHDB L0 reuse. Build a change-log part and a snapshot part with
  `DurableSubstrate`/`LocalFsSubstrate`, `PartWriter`/`SealedPart`, frames and
  `Manifest`. Record the fit and gaps in research.md R3. Verify that nothing is written
  to an EHDB instance or dataset.
- [ ] T007 `ehgraph-core`: model, ids, typed props, `ChangeRecord`, per-namespace
  `graph_version` (FR-001).
- [ ] T007a `ehgraph-server` instance config: `instance_id`, `domain`, `data_dir`,
  `backup_prefix`, budgets. The identity goes into every envelope, metric and log line
  (FR-022).
- [ ] T008 `ehgraph-store`: append-only durable change log, head pointer, and rebuild of
  materialized state from snapshot + log (FR-002). Tests: crash between append and
  acknowledgement; delete derived state → identical results (SC-001, SC-002).
- [ ] T008a Single writer by construction: data-dir lock. Test: a second process on the
  same `data_dir` refuses to start (FR-025).
- [ ] T008b Age-bounded seal (EHDB `seal_max_age` pattern; off by default in EHDB, on in
  ehgraph), so the unshipped tail is time-bounded (R4).

## Phase 3: User Story 1 — durable, idempotent writes (P1)

- [ ] T009 [US1] Batch upsert and deactivate; idempotency keys; version returned
  (FR-007). Test: same batch twice → same state and version.
- [ ] T010 [US1] Soft history (`first_seen`, `last_seen`, `active`) and re-activation;
  deactivate by `{source, not_seen_since}`.
- [ ] T011 [US1] Namespace isolation and the schema config hook (FR-001; mode decided by
  T001).

## Phase 4: User Story 2 — versioned reads over MCP/HTTP (P1)

- [ ] T012 [US2] Versioned snapshots: Arrow IPC parts in the manifest, scope export
  (full/filter/k-hop), retention (FR-003).
- [ ] T013 [US2] `ehgraph-api`: node, edge, query, neighbors, subgraph, path and
  snapshot.info over the durable store at head or at a version (FR-005).
- [ ] T014 [US2] `ehgraph-server`: MCP JSON-RPC (`tools/list`, `tools/call`) + HTTP, the
  result envelope and error codes per contracts/graphdb.md (FR-006).
- [ ] T015 [US2] Auth: keychain-issued credentials; read/write grants per namespace
  (FR-008).
- [ ] T016 [US2] Contract test: a NoETL playbook (`kind: mcp`) on the local runtime
  upserts, then calls every tool in contracts/graphdb.md (SC-007).

## Phase 5: User Story 3 — session-scoped shared-memory graph (P1)

- [ ] T017 [US3] `ehgraph-memory`: session registry and state machine
  (loading/active/refreshing/closed/expired/evicted/failed) (FR-010, FR-013, FR-014).
- [ ] T018 [US3] Snapshot loader: Arrow batches + CSR adjacency + id index. Single-flight
  load per `(owner, scope)`, so concurrent first reads share one load (FR-011, FR-012).
- [ ] T019 [US3] Read API served from session memory via `graph_session_id`. Durable
  fallback when no session is given (FR-018).
- [ ] T020 [US3] Refresh with atomic swap; pinned version per session (FR-015).
- [ ] T021 [US3] Global and per-session budgets: evict only closed or expired sessions;
  explicit `MEMORY_BUDGET_EXCEEDED`; concurrent-session isolation and optional sharing
  (FR-016, FR-017).
- [ ] T022 [US3] Idle lease and max lifetime; release within the grace period (FR-014,
  SC-006).
- [ ] T023 [US3] Read-only enforcement (`READ_ONLY_SESSION`) (FR-019). Lifecycle events
  and metrics (FR-020).
- [ ] T024 [P] [US3] Optional: place batches in POSIX shm in the `noetl-arrow-cache`
  `IpcHint` format for same-node attach. Document `/dev/shm` sizing (R2).
- [ ] T025 [US3] Tests: one durable read per load and the same version across readers
  (SC-005); concurrent first load; idle expiry; budget overflow; refresh during reads.
- [ ] T026 [US3] Benchmarks for load latency and footprint on the small and scale
  fixtures (SC-003, SC-004), with thresholds from T001.

## Phase 6: User Story 4 — operate instances as NoETL catalog resources (P2)

- [ ] T027 [P] [US4] `/healthz` (instance_id, domain, role, state) and `/metrics`, with
  every series labelled by instance (FR-009, FR-022). Add the
  `graph.instance.info` tool.
- [ ] T028 [US4] Container image. Per-instance templates under `deploy/`: StatefulSet
  `ehgraph-<domain>` (`replicas: 1`, RWO volume), Service, config.
- [ ] T029 [US4] `playbooks/mcp_resource.template.yaml` plus one `kind: Mcp` resource per
  initial instance: `ehgraph-noetl-cluster` and `ehgraph-network` (Q20). Shared
  lifecycle playbooks `deploy|undeploy|redeploy|restart|status|discover`, parameterized
  by instance (FR-023).
  - Verify the field names and how lifecycle dispatch passes the resource, against
    `docs/docs/architecture/mcp_catalog_architecture.md`.
  - If dispatch does not pass it, use per-instance wrapper playbooks.
- [ ] T029a [US4] `ehgraph-backup`: ship sealed parts, snapshots and the manifest to the
  object store, head pointer last. Add the `graph.backup.status` tool and the lag metrics
  (FR-024).
- [ ] T029b [US4] `ops/backup` and `ops/restore` playbooks.
  - Test: discard the volume, restore, and get identical results at the backed-up
    version (SC-008).
  - Test: two instances side by side stay isolated across write, restore and undeploy
    (SC-009).
- [ ] T030 [US4] Local-run path for laptops: start/stop script (one process per
  instance, with its own data dir and port) and a README section, so consumer fixture
  runs can start ehgraph first (R2 gap 6).

## Phase 6b: Instance-model roadmap (planned, NOT MVP; gated on Q18)

- [ ] T034 Stage 1 design: a read replica tails the change log from a `graph_version`
  cursor (EHDB `ChangeFeed`/`ehdb-feed` shape) and serves sessions and reads. Writes are
  rejected with a primary hint.
- [ ] T035 Stage 1 design: warm standby with manual or owner-gated promotion. This needs a
  K8s Lease election with an epoch fencing token, and store-side fencing (Invariant F,
  EHDB `docs/spec/writer-election-and-fencing.md`). Do not enable replicas before both
  exist.
- [ ] T036 Record the measured trigger for Stage 1 (RTO, or read/session load beyond one
  node) and the reasons sharding stays deferred (R4).

## Phase 7: Polish

- [ ] T031 [P] Wiki pages: Home, Architecture, Tool contract, Operations
  (`noetl/ehgraph.wiki`).
- [ ] T032 [P] ai-meta record: memory/current.md entry; `.vscode/settings.json`
  `rust-analyzer.linkedProjects` and `agents/rules/handoff-routing.md` once `Cargo.toml`
  exists.
- [ ] T033 First consumer integrations:
  - qnetmon (network domain): hand its session playbooks and tool playbooks the contract,
    with `ehgraph-network` as the resource. qnetmon's tasks reference this spec.
  - A NoETL services/cluster topology on `ehgraph-noetl-cluster`: define its schema
    config with the NoETL owners (Q20).

## Dependencies

- T001 → all.
- Phase 2 → US1, US2, US3.
- US1 → US2 (reads need data). T012 → US3 (the loader reads snapshots).
- US2 server (T014) → US3 network reads and US4.
- T008 → T029a (backup ships what the store seals). Phase 6b → after the MVP and Q18.
- [P] tasks within a phase can run in parallel.
