# Getting Started

You run one ehgraph process per domain. Two domains on one laptop are two processes, two data directories, and two ports.

## Build

Rust 1.99.1 (see `rust-toolchain.toml`).

```bash
cargo build -p ehgraph-server
```

The binary is `target/debug/ehgraph`.

## Config

Copy `deploy/config.example.toml`. The fields that identify the instance are `instance_id`, `domain`, `data_dir`, and `backup_prefix`. Every response and metric includes `instance_id` and `domain`.

| Field | Meaning |
|---|---|
| `instance_id` | Stable name. Convention: `ehgraph-<domain>`, for example `ehgraph-network`. |
| `domain` | `network`, `noetl-cluster`, or another label you choose. |
| `data_dir` | This instance's volume. Never share it. |
| `backup_prefix` | Directory the shipper copies sealed parts into. |
| `listen` | HTTP listen address. MCP is `POST /mcp` on this port. |
| `role` | `primary`. `replica` and `standby` refuse to start. |
| `backup_interval_s` | `0` ships only when you ask. A positive value ships on that interval. |
| `[budgets]` | Global session bytes and the per-session cap. Defaults are 256 MiB and 64 MiB. |
| `[session]` | Idle timeout (300s), max lifetime (24h), grace before memory is freed (2s). |
| `[store]` | Seal limits. `seal_max_age_s` defaults to 30. |
| `[auth]` | `bearer` or `disabled`. |

Auth grants are bearer tokens. `namespace = "*"` is an operator grant (backup and every namespace). A grant is read and/or write for one namespace. `disabled` is for a laptop. Health and metrics do not require a token. Tool calls do, unless auth is disabled.

An optional schema file `data_dir/schemas/<namespace>.json` turns on validation. Without it the namespace is schemaless. With `"strict": true`, only declared types are accepted and `required` properties must be present.

## One process per domain

```bash
scripts/ehgraph-local.sh start network
scripts/ehgraph-local.sh start noetl-cluster
scripts/ehgraph-local.sh status network
scripts/ehgraph-local.sh stop network
```

`network` listens on `127.0.0.1:8081`. `noetl-cluster` listens on `127.0.0.1:8082`. Data lives under `.local/ehgraph-<domain>/` unless you set `EHGRAPH_HOME`. The generated config uses `auth.mode = "disabled"`.

```bash
curl -s http://127.0.0.1:8081/healthz
```

```json
{"instance_id":"ehgraph-network","domain":"network","role":"primary","state":"serving"}
```

Write a node (auth disabled, so no header):

```bash
curl -s -X POST http://127.0.0.1:8081/v1/tools/graph.upsert \
  -H 'content-type: application/json' \
  -d '{"namespace":"qnetmon","source":"manual","execution_id":"1","nodes":[{"id":"device:sw1","type":"Device","props":{"site":"s1"}}],"edges":[]}'
```

Open a session and read from it:

```bash
curl -s -X POST http://127.0.0.1:8081/v1/tools/graph.session.open \
  -H 'content-type: application/json' \
  -d '{"namespace":"qnetmon","owner":"alice","scope":{"kind":"full"}}'
```

Pass the returned `graph_session_id` on later read tools. The version stays pinned until `graph.session.refresh` or `graph.session.close`.

With bearer auth, send `Authorization: Bearer <token>`.

## MCP

`POST /mcp` speaks JSON-RPC. `initialize`, `tools/list`, and `tools/call` are implemented. `GET /mcp/tools` is the catalog discovery URL. A NoETL playbook selects the domain by calling the resource `ehgraph-<domain>` at `mcp/ehgraph/<domain>`, not by a field inside ehgraph.

## Backup on a laptop

```bash
ehgraph backup --config .local/ehgraph-network/config.toml
```

Stop the server first. The server holds the data-directory lock, so a second process cannot open it. On a running server, `POST /v1/admin/backup` with an operator token does the same ship inside the process.

Restore only into an empty directory:

```bash
ehgraph restore --config /path/to/config.toml
ehgraph serve --config /path/to/config.toml
```

`data_dir` must be empty. `backup_prefix` is the directory you shipped to.
