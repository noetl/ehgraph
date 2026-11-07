# Performance

Targets per tool class (register C17), how they are measured, and the latest numbers. The harness is `crates/ehgraph/benches/perf.rs`:

```bash
cargo bench -p ehgraph --bench perf -- --elements 100000
cargo bench -p ehgraph --bench perf -- --elements 50000 --check --json perf.json   # what CI runs
```

It builds a `noetl-cluster`-shaped graph: services, pods and nodes with 5 properties each, and two edges per node. It goes through the same in-process dispatch that both modes use, and reports wall-clock p50 and p95 over repeated calls. `--check` marks any case whose p95 misses its target with `REGRESSION`. `--strict` also makes the run exit non-zero.

## Targets

Targets are for **10^6 elements in one namespace on one node**, and they apply in both modes. Cases marked *scales* are linear in the graph, so a smaller run is checked against the target scaled to its size.

| Case | Target (p95) | Notes |
|---|---|---|
| `graph.node` / `graph.edge`, sessionless at head | ≤ 5 ms | Read straight from the live graph, with no copy |
| `graph.node` in a session | ≤ 1 ms | |
| `graph.node` via `call_async` (embedded) | ≤ 5 ms | Includes the hop to the host's blocking pool |
| `graph.neighbors` (limit 100), head view warm | ≤ 10 ms | |
| `graph.subgraph` k=2, max 500 nodes | ≤ 50 ms | |
| `graph.neighbors` right after a write | ≤ 2 s (*scales*) | Rebuilds the cached head view; open a session for read-heavy work on a changing graph |
| `graph.upsert`, 10 nodes, warm | ≤ 20 ms | fsync-bound; independent of graph size |
| Bulk load (batches of 1,000 nodes / 2,000 edges) | ≥ 20,000 elements/s | |
| `graph.session.open`, full scope | ≤ 5 s (*scales*) | |
| Open / replay | ≤ 30 s (*scales*) | Compaction (C02) bounds it to a snapshot plus the tail |
| Resident graph | ≤ 2,000 B/element | Cluster shape; see Operations, "Memory" |

**Regression budget:**

- CI runs the harness at 50,000 elements on every push, as the `bench` job.
- The job is **non-gating**: it is `continue-on-error`, and it uploads `perf.json`.
- A case over target prints `REGRESSION` in the job log.
- Make it gating (`--strict`) once runner variance is known.

## Measured

Apple M1 Max (10 cores), APFS. On macOS, fsync is a full device flush, which slows the write cases. Release build, 2026-11-06.

| Case | 100k: before | 100k: after | 300k: after |
|---|---|---|---|
| `graph.node` sessionless | 119 ms | < 0.01 ms | < 0.01 ms |
| `graph.neighbors` | 119 ms | 0.01 ms | 0.01 ms |
| `graph.subgraph` k=2 | 120 ms | 0.03 ms | 0.03 ms |
| `graph.neighbors` right after a write | (119 ms) | | 424 ms |
| `graph.node` via `call_async` | | | 0.03 ms |
| `graph.upsert` 10 nodes | 78 ms | 15 ms\* | 15 ms |
| Bulk load | 26k el/s | 47k el/s | 60k el/s |
| `graph.session.open` full | 153 ms | 184 ms | 519 ms |
| Open / replay | 243 ms | 264 ms | 740 ms |
| Resident | 1,037 B/el | 1,037 B/el | 1,037 B/el |

\* After the derived-cache marker stopped being fsynced. Before that change, the same run gave 25 ms.

The fixes behind the "after" columns (noetl/ehgraph#7):

- **Sessionless reads at head** no longer rebuild a query view of the whole namespace on every call.
  - `graph.node` and `graph.edge` read the live graph directly.
  - The other read tools use a cached head view. It is rebuilt only when the head moves, and kept only up to `per_session_bytes`. Its size is reported as `ehgraph_head_view_bytes`.
- **`graph.upsert` plans against an overlay** of the batch's own images instead of cloning the namespace. Before this change every write was O(namespace).
- **The derived-cache marker** is no longer fsynced. Nothing reads it back, and it accounted for 2 of the 5 fsyncs per commit.

Extrapolated to 10^6 elements on the same machine: replay about 2.5 s, a full session open about 1.7 s, and a neighbors read right after a write about 1.3 s. All are inside their targets. A run at the full 10^6 should be done on production-like hardware, which is a Linux persistent disk, before setting the targets as SLOs.
