# Data Model: ehgraph MVP

The model is generic. Consumer-specific node/edge types live in the consumer's schema
config. qnetmon's types (Device, Interface, Site, Segment, Service; CONNECTED_TO,
HAS_INTERFACE, ...) are defined in qnetmon, not here.

## Property-graph model

| Element | Fields |
|---|---|
| Namespace | `name`, `schema_ref?` (FR-001, NEEDS CLARIFICATION), `head_version`, grants (read/write principals) |
| Node | `id` (stable string chosen by the consumer), `type`, `props` (typed map), `first_seen`, `last_seen`, `active`, `origin?` (consumer-defined tag, e.g. `discovered` \| `designed`), `source`, `execution_id`, `version_created`, `version_updated` |
| Edge | `id` (consumer-supplied, or default `hash(src, type, dst, origin)`), `src`, `dst`, `type`, `props`, plus the same history and provenance fields as Node |
| ChangeRecord | `graph_version` (u64, monotonic per namespace), `op` (`upsert_node`, `upsert_edge`, `deactivate_node`, `deactivate_edge`), element payload, `idempotency_key?`, `execution_id`, `ts` |
| Snapshot | `snapshot_id`, `namespace`, `graph_version`, `scope` (full \| property filter \| k-hop seeds), part list, node/edge counts, bytes, `created_at`; listed in a versioned manifest |
| GraphSession | `graph_session_id`, `owner` (FR-010), `namespace`, `scope`, `snapshot_version`, `state`, `opened_at`, `last_read_at`, `idle_timeout_s`, `max_lifetime_s`, `resident_bytes`, `host_node` |

Rules:

- Ids are deterministic and supplied by the consumer, which makes upserts idempotent.
- An edge upsert whose `src` or `dst` is unknown or inactive is rejected with
  `INVALID_ARGUMENT`. [NEEDS CLARIFICATION: or create placeholder nodes?]
- Props are typed: string, int, float, bool, timestamp, list of scalar, nested map.
  [NEEDS CLARIFICATION: is a nested map allowed in the MVP?]
- An idempotency key is remembered per namespace for [NEEDS CLARIFICATION: window].

## Durable layout (proposed; research.md R3)

- **Change log**: immutable parts on an EHDB L0 `DurableSubstrate` (`LocalFsSubstrate`
  first). Records are framed (`magic+len+crc32+body`). Sort key = `graph_version`,
  index key = element id.
- **Head pointer**: `put_overwrite` for the namespace's latest durable `graph_version`
  and manifest version.
- **Snapshots**: immutable Arrow IPC part files (node batch and edge batch), written with
  `put_if_absent` (or to a content-addressed `ImmutableObjectStore`) and listed in a
  versioned `Manifest`.
- **Materialized state for durable reads**: rebuilt from the latest snapshot plus the log
  tail. It is never authoritative.
- **Retention**: snapshots older than the policy are dropped from the manifest. Log
  segments covered by a retained snapshot may be compacted. [NEEDS CLARIFICATION FR-003]

## In-memory layout (graph sessions)

- **Element data**: Arrow `RecordBatch`es for nodes and edges.
- **Adjacency**: a CSR adjacency index (out and in) over dense node ordinals, plus an
  id→ordinal hash map. A per-type edge offset is optional.
- **Optional same-node attach**: batches placed in POSIX shm in the `noetl-arrow-cache`
  `IpcHint` format, so colocated processes can attach zero-copy. Cross-node attach is
  never used.
- **Sharing**: read-only, one copy per session (or reference-counted across identical
  `(namespace, scope, version)` — FR-017). Released when the session ends.

## State transitions

- **Node/edge**:
  - active → inactive on deactivate.
  - inactive → active on re-upsert.
  - Never hard-deleted in the MVP.
- **GraphSession**:
  - loading → active → (refreshing → active)* → closed | expired | evicted.
  - loading → failed.
  - Only `active` serves reads. `refreshing` serves the old version until the swap.
  - `evicted` applies only to sessions that have already ended or idle-expired, or to an
    explicit admin action (FR-016).

```text
          open/first read
               │
            loading ──fail──► failed
               │ loaded (version v)
               ▼
   ┌────────► active ◄──── swap ────┐
   │           │  │                  │
   │  read     │  └──refresh──► refreshing
   │ (renew)   │
   └───────────┤
               ├─ close ───────► closed
               ├─ idle/max ────► expired
               └─ admin ───────► evicted
```
