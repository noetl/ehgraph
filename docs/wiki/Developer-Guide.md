# Developer Guide

## Crates

The workspace produces one binary, `ehgraph`, from `ehgraph-server`. The other crates are internal.

| Crate | Responsibility |
|---|---|
| `ehgraph-core` | Ids, typed properties, nodes and edges, change records, scopes, errors, the result envelope |
| `ehgraph-store` | Data-directory lock, framed change log, snapshots (Arrow IPC), rebuild, schema hook |
| `ehgraph-backup` | Copy sealed parts to a directory prefix and restore into an empty directory |
| `ehgraph-memory` | Session registry, Arrow batches, CSR adjacency, budgets, idle and lifetime |
| `ehgraph-api` | Tool dispatch over the store or a session |
| `ehgraph-server` | Config, bearer grants, HTTP, MCP, metrics, the `ehgraph` binary |

`ehgraph-hold` is a test helper in the store crate. It is not part of the server surface.

EHDB is a design reference for frames, sealed parts, the manifest, and the age seal. It is not a Cargo dependency. See research R3 in `specs/001-ehgraph-mvp/research.md`.

## Layout on disk

```text
data_dir/
  LOCK
  instance.json
  schemas/<namespace>.json          optional
  namespaces/<namespace>/
    head.json
    manifest.json
    changelog/part-00000001.sealed
    changelog/active.part
    snapshots/<id>/{nodes.arrow,edges.arrow,meta.json}
    derived/cache.json              safe to delete
```

## Build, test, lint

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI is `.github/workflows/ci.yml` and runs those three on Rust 1.99.1.

Tests that matter for the guarantees:

- crash between append and acknowledgement, and a torn tail (`ehgraph-store` durability)
- rebuild after deleting `derived/`
- idempotent replay and idempotency-key conflict
- second process on the same data directory
- age seal
- strict schema
- session single-flight, pinned refresh while a read is in flight, idle expiry, budget overflow
- backup restore and two instances side by side
- HTTP and MCP contract walk of every tool (`ehgraph-server` contract test)

Fixtures: `tests/fixtures/small/graph.json` is checked in. `tests/fixtures/scale/spec.json` describes the 10_000 node / 20_000 edge graph the memory test generates. Smoke thresholds, not production SLOs: scale load under 5 seconds.

## Contributing

1. Branch from the current spec branch when the change is still spec-driven, otherwise from `main`.
2. Keep domain types out of ehgraph. A new node type belongs in the consumer schema, not in a `match` on `"Device"`.
3. A mutation is a change-log record before it is acknowledged. Do not add a write path that updates a session graph in place.
4. Do not start a replica. `role = replica` must keep failing until election and store-side fencing exist.
5. Do not add EHDB as a dependency unless Q15 is closed with the EHDB maintainers. Extending the local log is fine. The frame and the `graph_version` cursor should stay recognizable to a future tailer.
6. Update the wiki pages under `docs/wiki/` in the same change. Those files are the GitHub wiki, one page per file.

## Spec

`specs/001-ehgraph-mvp/` is the MVP spec, plan, research, data model, and tool contract. Open questions that this code had to answer are recorded as provisional decisions in `research.md`. They are not constitution amendments.
