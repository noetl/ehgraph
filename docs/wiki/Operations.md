# Operations

## Health and metrics

`GET /healthz` returns `instance_id`, `domain`, `role`, and `state` (`serving` once the process holds the lock). The lifecycle `status` playbook curls this URL.

`GET /metrics` is Prometheus text. Every series has `instance_id` and `domain`.

| Series | Meaning |
|---|---|
| `ehgraph_sessions{state}` | Sessions in that state |
| `ehgraph_resident_bytes` | Bytes held by session graphs |
| `ehgraph_load_seconds` | Most recent session load, in seconds |
| `ehgraph_evictions_total{reason}` | Sessions dropped under budget pressure (`budget`) |
| `ehgraph_resident_graph_bytes{namespace}` | Estimated resident bytes of the materialized graph |
| `ehgraph_budget_bytes{kind}` | Resolved budgets: `resident_graph` (0 = unlimited), `sessions_global`, `session` |
| `ehgraph_graph_version{namespace}` | Durable head |
| `ehgraph_backup_lag_versions{namespace}` | Head minus the last shipped sealed version |
| `ehgraph_backup_lag_seconds{namespace}` | Time since that ship, when the lag is non-zero |
| `ehgraph_backup_failures_total` | Ships that failed since open |
| `ehgraph_backup_last_shipped_timestamp_seconds` | Unix time of the last checkpoint; 0 if never shipped |
| `ehgraph_idempotency_keys{namespace}` | Idempotency keys remembered (bounded, C23) |
| `ehgraph_idempotency_key_bytes{namespace}` | Their estimated resident bytes |
| `ehgraph_idempotency_keys_evicted{namespace}` | Keys forgotten since open (replay included) |

Session lifecycle is also written to the process log: opened, loaded, refreshed, closed, expired, evicted, load failed. The fields are session id, owner, namespace, version, and bytes.

## Kubernetes

`deploy/template/statefulset.yaml` is one StatefulSet and Service. `replicas` is 1. The volume is ReadWriteOnce. Render it with:

```bash
scripts/render-deploy.sh network
```

This is for standalone domains only. `noetl-cluster` is embedded in noetl-server (E4), so it has no StatefulSet. The script and the mutating lifecycle playbooks refuse it, and `deploy/instances/ehgraph-noetl-cluster/README.md` explains why. Its graph is served at `http://noetl.noetl.svc.cluster.local:8082/ehgraph/noetl-cluster`.

The rendered copy for `network` is under `deploy/instances/`. CI checks that it matches the template. The container image is built from the repository `Dockerfile` (`ehgraph serve --config /etc/ehgraph/config.toml`).

The Service DNS used by a standalone domain's catalog resource is `ehgraph-<domain>.ehgraph.svc.cluster.local:8080`.

## Change-log compaction

The change log is the authority, so without compaction it grows for the life of a namespace, and so does replay on open. With compaction on, sealed parts are folded into a full snapshot after each successful backup:

```toml
[store]
compaction = true      # off by default
compact_min_parts = 8  # compact only when at least this many parts can go
```

A prefix of sealed parts is dropped only when every part in it meets all three conditions:

- it is **shipped**: at or below the backup checkpoint's `backed_up_version`. A part that was never backed up is never dropped;
- it is **covered** by a full snapshot written exactly at the new floor version;
- it is **outside the idempotency window**, or records no idempotency key. Its keys are already forgotten (C23), so the key window is unchanged.

Compaction is crash-safe. The steps run in this order:

1. the floor snapshot is made durable;
2. the new manifest, carrying `floor_version` and the pinned `floor_snapshot`, is written;
3. only then are the part files deleted.

A crash before step 2 leaves the old log in force. A crash after it leaves part files that the next open removes rather than re-adopts. The same applies to a restore from a backup prefix, which keeps every part it ever received.

What changes for readers and operators:

- **History:** `graph_at` / `graph.version.info` at a version **below the floor** returns `NOT_FOUND`. The floor itself and everything after it stay readable.
- **Snapshot retention** (`snapshot_keep`) never removes the floor snapshot. It now retires snapshots only after the new manifest is durable.
- **Format:** a compacted manifest is format 2. Older builds refuse it (E9).
- **Metrics** (per namespace): `ehgraph_log_parts`, `ehgraph_log_bytes`, `ehgraph_log_floor_version` and `ehgraph_open_replay_seconds`.

Compaction runs under the backup's ship lock, so it never deletes a part that a ship is still copying.

## Memory

An instance holds three kinds of memory:

| Memory | Budget | On overflow |
|---|---|---|
| Materialized graph, every namespace, always resident | `[budgets] resident_graph_bytes` | A growing write is refused with `MEMORY_BUDGET_EXCEEDED` before it is logged. Writes that don't grow it, and opening, are never refused. |
| Sessions (Arrow batches) | `global_bytes`, `per_session_bytes` | Ended sessions are evicted first, then the open fails with `MEMORY_BUDGET_EXCEEDED` |
| Idempotency keys | `[store] idempotency_max_keys` | The oldest keys are forgotten (C23) |

Defaults:

- **Standalone:** 256 MiB for sessions in total, 64 MiB per session, and an unlimited graph.
- **Embedded:** derived from the host's limit (see the Embedding page). Open refuses budgets that would take more than half the host.

Measured resident sizes come from `ehgraph-store/tests/resident_bytes.rs`, which counts allocations and checks that the estimates stay within 25%. The figures are requested heap; budget about 20% more for allocator overhead.

| Shape | Bytes per element |
|---|---|
| `noetl-cluster`-like: 6 properties including a 2-entry label map; edges carry 1 property | about 1,660 |
| Bare: no properties | about 670 |

So 10^4 cluster elements take about 16 MiB, and 10^6 take about 1.6 GiB. Each non-empty property map costs a whole `BTreeMap` leaf, about 630 B, so nested maps are the expensive part.

Writes plan against the live graph by reference. Earlier builds cloned the whole namespace on every upsert, which doubled peak memory per write.

## Backup and restore

`backup_prefix` picks the target:

| `backup_prefix` | Target |
|---|---|
| `/var/backups/ehgraph/ehgraph-network` | a directory (a mounted volume) |
| `gs://noetl-ehgraph/ehgraph-network` | a Google Cloud Storage prefix |

Both targets follow the same rules:

- **Immutable objects** are put-if-absent. These are sealed parts, snapshots, `instance.json` and schema files. On GCS that is an upload with `ifGenerationMatch=0`, so a retried or racing ship never rewrites one.
- **Per namespace**, the manifest is written after the parts it names, and the head after the manifest.
- **`checkpoint.json` is written last.** A crash mid-ship leaves the previous checkpoint, and every object it names exists.

A GCS target is configured in `[backup]`:

```toml
[backup]
endpoint = "https://storage.googleapis.com"   # override only for an emulator
auth = "metadata"                             # or "env:VAR", or "none"
timeout_s = 60
```

- **`auth = "metadata"`** (the default) uses the GKE metadata server, which is Workload Identity. The pod's Kubernetes service account needs `roles/storage.objectCreator` and `roles/storage.objectViewer` on the bucket. `storage.objects.delete` is not needed: ehgraph never deletes from a backup prefix.
- **Embedded:** inside noetl-server the graph uses the same pod identity that noetl-server's own `gcp_auth` uses, so there is no second credential. A host can also pass its own token source through `ehgraph::OpenOptions::backup_token`.
- **Retries:** transient errors (timeouts, 408, 429, 5xx) are retried 4 times with backoff. A 401 or 403 is reported as `UNAUTHORIZED` and names the missing permissions.

**RPO and RTO.**

- **RPO:** the unshipped tail is bounded by `backup_interval_s + seal_max_age_s`. That is 60 s with the example config, and it is the RPO target.
- **RTO:** the time to list and download the prefix, plus replay on open. The target is 15 minutes for one domain at the planning size of about 10^6 elements. It has not been measured against a real bucket yet (C17).
- **Alerting:** `deploy/alerts/ehgraph-backup.rules.yaml` alerts on lag (more than 5 times the RPO), on repeated failures (`ehgraph_backup_failures_total`), and on an instance that has never shipped (`ehgraph_backup_last_shipped_timestamp_seconds == 0`).

Ship from inside the running process:

- `backup_interval_s` in the config, or
- `POST /v1/admin/backup` with an operator token, or
- the playbook `automation/agents/ehgraph/ops/backup`

Ship from a stopped process:

```bash
ehgraph backup --config /etc/ehgraph/config.toml
```

The second form takes the data-directory lock, so it fails while the server is up. That is the single-writer rule.

Restore is offline, from either target. It reads and format-checks `checkpoint.json` before it writes anything (E9), and it only reads objects under its own prefix. Empty the volume, then:

```bash
ehgraph restore --config /etc/ehgraph/config.toml
```

Then start the server. The playbook `automation/agents/ehgraph/ops/restore` scales the StatefulSet to 0 and back to 1 around that command. A restored instance comes up at the backed-up version. Later writes continue from there. Restoring one instance does not touch another instance's directory or prefix.

## What the lag means

Backup lag is the versions still in an unsealed part, plus anything sealed but not yet copied. Age seal (default 30s) bounds how long a record can sit unsealed. The reported lag is the RPO. There is no numeric RTO in this release. Restore time is the copy plus the rebuild.

## Failure you should see, not infer

- A second process on the same `data_dir` exits with a lock error.
- A crash after the frame is on disk and before acknowledgement comes back as a fully applied batch, or not at all.
- Deleting derived cache files does not change query results after restart.
- An active session is never dropped to free memory. The caller gets `MEMORY_BUDGET_EXCEEDED`.
