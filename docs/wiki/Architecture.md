# Architecture

ehgraph is one topology engine, the `ehgraph` crate, that runs in two modes (see [Embedding](Embedding)):

- **Standalone:** the `ehgraph` server binary, one process per topology domain.
- **Embedded:** in-process inside a host such as noetl-server.

Both modes use the same log, tools, grants and guarantees. The crates behind `ehgraph` (`-core`, `-store`, `-memory`, `-api`, `-backup`) are internal.

> Embedded mode is proposed with constitution 3.0.0 and is not ratified yet.

## Identity

```text
instance (instance_id, domain)          ehgraph-network
  └── namespace                         qnetmon
        └── node or edge                device:sw1
```

The domain is a label set at deploy time (`network`, `noetl-cluster`). ehgraph has no domain-specific node types. Those live in the consumer's schema file and playbooks. A global address used only in logs is `<instance_id>/<namespace>/<node|edge>:<id>`. APIs take `namespace` plus `id`. You pick the instance by calling its `kind: Mcp` resource.

## Change log

The log is the authority. Each namespace has its own monotonic `graph_version`. A batch is one framed record. The frame is `magic || version || length || crc32 || json body`, the same shape EHDB L0 uses, with ehgraph's own magic (`EHGR`). This process does not link EHDB and does not write an EHDB dataset.

A write is acknowledged only after the frame is fsynced and the head pointer is published. If the process dies after the frame is durable and before the head file moves, the next open fast-forwards the head. A torn tail is truncated and ignored. Replaying a batch with the same idempotency key does not change state or version. An identical upsert with no key is also a no-op.

Materialized state and session graphs are derived. Deleting `namespaces/<name>/derived/` and reopening rebuilds from full snapshots plus the log.

Parts seal on size, record count, or age (`seal_max_age`, on by default, 30 seconds). Sealed parts are immutable. That age bound is the unshipped tail a backup can still lose.

## Sessions

A graph session is opened explicitly or on first use. The server loads the requested scope once (full namespace, a property filter, a k-hop neighbourhood, or a named snapshot), builds Arrow record batches plus a CSR adjacency index, and pins that `graph_version`. Later reads in the session do not touch the log. A refresh builds a new graph and swaps it. A read that started before the swap finishes on the old version.

Sessions are read-only. A write that names a `graph_session_id` returns `READ_ONLY_SESSION`. Writes go to the log and show up in the session only after refresh.

Memory is bounded by a per-session cap and a global budget. On overflow the server drops only sessions that are already closed or expired, then returns `MEMORY_BUDGET_EXCEEDED`. It does not evict an active session. Idle timeout and a maximum lifetime end a session; a later read with that id returns `SESSION_EXPIRED`.

Copies are not shared across sessions. Two owners loading the same scope get two resident graphs. Concurrent first loads for the same owner and scope share one load.

## Single writer

One process holds an exclusive lock on `data_dir/LOCK`: the server, or the embedding host. A second process using the same directory is refused. So is a second handle in the same process, through a process-wide claim on the canonical path. Kubernetes deploys use a StatefulSet with `replicas: 1` and a ReadWriteOnce volume. The server refuses to start with `role` set to `replica` or `standby`.

## Backup

`ehgraph backup`, `POST /v1/admin/backup`, or `backup_interval_s` seals open parts and copies sealed parts, snapshot files, and the manifest to `backup_prefix`. The head pointer in the backup is the sealed version and is written last. Restore copies that tree into an empty data directory. Opening the store rebuilds.

The backup directory is a filesystem prefix. It stands in for an object store until an S3-compatible client is chosen. Immutable files are copied only if absent. The manifest is replaced. Nothing is written outside the instance data directory and that prefix.

## What is deliberately absent

There is no replica, no election, and no sharding. The log is still an ordered `graph_version` sequence, so a later follower can tail it. See [Roadmap](Roadmap).
