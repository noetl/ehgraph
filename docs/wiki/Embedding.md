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

- **One host process per instance.** The lock refuses a second handle in the same process
  and a second process on the same directory. It cannot see two pods that embed the same
  `instance_id` on two different volumes. That is two primaries for one domain. If the
  host runs several replicas, embed on one of them only, or keep that domain standalone.
- **Use the async entry points** from async code (`open_embedded_async`, `call_async`,
  `Client::call_async`). Writes fsync.
- **Call `start_maintenance`.** Without it, idle sessions never expire and the budget
  fills up.
- **Size the budget** (`[budgets]`) inside the host's memory limit.
- **Auth stays on.** `auth.mode = "disabled"` is refused when embedded, unless
  `auth.allow_disabled_embedded = true` is also set for a local-only host.
- **Backups:** `backup_interval_s`, `EhGraph::backup_now`, or `POST <prefix>/v1/admin/backup`
  (operator grant). The `ehgraph backup` CLI cannot run while the host holds the lock.
- **Upgrades and rollbacks** go with the host's release. See research E9 before the first
  on-disk format change.
