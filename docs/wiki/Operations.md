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
| `ehgraph_graph_version{namespace}` | Durable head |
| `ehgraph_backup_lag_versions{namespace}` | Head minus the last shipped sealed version |
| `ehgraph_backup_lag_seconds{namespace}` | Time since that ship, when the lag is non-zero |

Session lifecycle is also written to the process log: opened, loaded, refreshed, closed, expired, evicted, load failed. The fields are session id, owner, namespace, version, and bytes.

## Kubernetes

`deploy/template/statefulset.yaml` is one StatefulSet and Service. `replicas` is 1. The volume is ReadWriteOnce. Render it with:

```bash
scripts/render-deploy.sh network
```

This is for standalone domains only. `noetl-cluster` is embedded in noetl-server (E4), so it has no StatefulSet. The script and the mutating lifecycle playbooks refuse it, and `deploy/instances/ehgraph-noetl-cluster/README.md` explains why. Its graph is served at `http://noetl.noetl.svc.cluster.local:8082/ehgraph/noetl-cluster`.

The rendered copy for `network` is under `deploy/instances/`. CI checks that it matches the template. The container image is built from the repository `Dockerfile` (`ehgraph serve --config /etc/ehgraph/config.toml`).

The Service DNS used by a standalone domain's catalog resource is `ehgraph-<domain>.ehgraph.svc.cluster.local:8080`.

## Backup and restore

Ship from inside the running process:

- `backup_interval_s` in the config, or
- `POST /v1/admin/backup` with an operator token, or
- the playbook `automation/agents/ehgraph/ops/backup`

Ship from a stopped process:

```bash
ehgraph backup --config /etc/ehgraph/config.toml
```

The second form takes the data-directory lock, so it fails while the server is up. That is the single-writer rule.

Restore is offline. Empty the volume, then:

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
