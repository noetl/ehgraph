# Embedding ehgraph

> Embedded mode comes with constitution 3.0.0 (adopted 2026-11-06). Its defaults are
> provisional: E1–E9 and the Clarification register (C01–C27) in
> `specs/001-ehgraph-mvp/research.md` list what the owner may still revisit.

The same engine runs in two modes:

| | Embedded | Standalone |
|---|---|---|
| Opened by | the host: `EhGraph::open_embedded` | `ehgraph serve`: `EhGraph::open_standalone` |
| Host's own calls | in-process (`call`, `Client`) with no network hop | n/a |
| MCP / HTTP | the host mounts `graph.router()` under its prefix | its own listener (`listen`) |
| Runtime, tracing, signals | the host's | the binary's |
| Data-dir lock held by | the host process | the server process |
| Deploy | the host's release | StatefulSet `replicas: 1` |

The log format, tools, per-namespace grants, sessions and budgets are identical in both.

## In a host

```rust
let config = ehgraph::GraphConfig::from_toml(&host_config.ehgraph_toml)?;
let graph = ehgraph::EhGraph::open_embedded_async(config).await?; // locks data_dir, replays the log
graph.start_maintenance()?;                                       // session sweep + backup, on the host runtime

// In-process, same grants as HTTP:
let client = graph.client(&internal_token)?;
let env = client.call_async("graph.neighbors", serde_json::json!({"namespace": "cluster", "id": "svc:api"})).await;

// The same instance for `kind: mcp` playbooks:
let app = host_router.nest("/ehgraph/noetl-cluster", graph.router());

// On host shutdown:
graph.shutdown(std::time::Duration::from_secs(10)).await; // stop tasks, drain, release the lock
```

A runnable version is in `crates/ehgraph/examples/embed_in_host.rs`.

## Rules for a host

- **Single writer, by declaration (E2).** An embedded instance refuses to open unless the
  host declares it is the only writer:

  ```toml
  [embedded]
  host_replicas = 1   # required; anything else refuses
  host_ordinal = 0    # optional: a StatefulSet pod index; only 0 may write
  leader = true       # optional: if the host runs an election, only the leader may write
  ```

  The lock refuses a second handle on one directory (in this process or another), and a
  process-wide claim refuses a second embedded primary for the same `instance_id` or
  `domain`. Neither can see a second *pod* on its own volume, so embedded mode never relies
  on them across pods. **A domain whose host cannot declare a single writer runs
  standalone.** The declaration is not proof: a host that runs two replicas and declares one
  defeats it, so the host needs its own replica checks too (noetl-server refuses on
  sharding, NATS-KV coherence, execution affinity, or a StatefulSet ordinal > 0).
- **Domains (E4).** `noetl-cluster` is embedded in noetl-server at `/ehgraph/noetl-cluster`.
  `network` (qnetmon) runs standalone.
- **An embedded domain has no StatefulSet.** A standalone pod for the same domain would be
  a second primary on its own volume, invisible to both the lock and the claim. So for an
  embedded domain:
  - its `kind: Mcp` resource sets `spec.deployment.mode: embedded` and points at the host
    mount, for example `http://noetl.noetl.svc.cluster.local:8082/ehgraph/noetl-cluster/mcp`;
  - the resource has only the read-only `status` and `discover` verbs. Rolling the graph is
    rolling the host;
  - `scripts/render-deploy.sh` and the `deploy`, `undeploy`, `redeploy`, and `restart`
    playbooks refuse it (`EMBEDDED_DOMAINS`, and `mode: embedded`);
  - `crates/ehgraph/tests/deploy_manifests.rs` fails CI if a manifest for it reappears or
    the resource and the guards drift.
- **Use the async entry points** from async code (`open_embedded_async`, `call_async`,
  `Client::call_async`). Writes fsync.
- **Call `start_maintenance`.** Without it, idle sessions never expire and the budget
  fills up.
- **Budgets come out of the host's memory (C11).** Unless they are set explicitly, the
  embedded defaults are taken from the host's limit: `[budgets] host_memory_bytes`, or else
  the container's cgroup limit.
  - The resident graph gets 10% of the host.
  - Sessions get 10% in total, and a quarter of that per session.
  - Open **refuses** if the graph and session budgets together exceed half the host.
  - With no detectable limit, the fixed defaults apply and the graph is capped at 256 MiB.

  A write that would grow the graph past `resident_graph_bytes` is refused with
  `MEMORY_BUDGET_EXCEEDED`, and nothing is logged. See Operations, "Memory", for
  measured sizes.
- **Auth stays on.** `auth.mode = "disabled"` is refused when embedded, unless
  `auth.allow_disabled_embedded = true` is also set for a local-only host.
- **Backups:** `backup_interval_s`, `EhGraph::backup_now`, or `POST <prefix>/v1/admin/backup`
  (operator grant). The `ehgraph backup` CLI cannot run while the host holds the lock.
- **Upgrades and rollbacks** go with the host's release. Every on-disk JSON file carries a
  `format` marker and is decoded strictly (E9): rolling the host back past a format change
  makes ehgraph **refuse to open** with a message naming the file, rather than silently
  dropping what it doesn't understand. Roll forward, or restore a backup the older build
  took. Any change to a marked file's shape bumps `STORE_FORMAT`.

  Format 2 is a **compacted** manifest (C02). Each file is written at the lowest format
  that describes it, so a build before compaction existed can still open a data directory
  until compaction has actually run on a namespace. Compaction is off by default; turn it
  on only once rolling back past this release is no longer needed.
