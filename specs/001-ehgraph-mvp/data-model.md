# Data Model: ehgraph MVP

The model is generic across topology domains. Consumer-specific node/edge types live in
the consumer's schema config. Examples, defined by their consumers and not here:

- network (qnetmon): Device, Interface, Site, Segment, Service; CONNECTED_TO,
  HAS_INTERFACE, ...
- NoETL cluster: e.g. Service, Deployment, Pod, Worker pool, Queue/Stream, Database;
  DEPENDS_ON, RUNS_ON, PUBLISHES_TO, ...

## Identity hierarchy: instance → namespace → element

```text
ehgraph instance (instance_id, domain)        e.g. ehgraph-network
  └── namespace (unique within the instance)  e.g. qnetmon, qnetmon-designed-lab
        └── node / edge (stable id within the namespace)
```

- **Domain**: the purpose a topology serves (`noetl-cluster`, `network`, ...).
  - A domain is served by its own instance, and the MVP has exactly one instance per
    domain.
  - The domain is a deploy-time label. It is not a data type, and ehgraph applies no
    domain-specific logic.
- **Instance**: one ehgraph database server (single primary in the MVP).
  - Instances share nothing and never reference each other.
  - Cross-domain relationships are the consumer's concern. For example, a consumer may
    store a foreign reference such as `ext:network/qnetmon/device:sw1` as a property.
- **Global address** of an element, for logs and provenance only:
  `<instance_id>/<namespace>/<node|edge>:<id>`. APIs take `namespace` + `id`, and the
  instance is chosen by the `kind: Mcp` resource used.

## Property-graph model

| Element | Fields |
|---|---|
| Instance | `instance_id` (stable, e.g. `ehgraph-network`), `domain`, `role` (`primary`; later `replica` \| `standby`, research R4), `data_dir`, `backup_prefix` (object store), budgets (global session bytes, per-session cap), `started_at`, `build`. Configured at deploy time; reported by `/healthz`, `status` and every envelope |
| Namespace | `name`, `schema_ref?` (FR-001, provisional C01), `head_version`, grants (read/write principals) |
| Node | `id` (stable string chosen by the consumer), `type`, `props` (typed map), `first_seen`, `last_seen`, `active`, `origin?` (consumer-defined tag, e.g. `discovered` \| `designed`), `source`, `execution_id`, `version_created`, `version_updated` |
| Edge | `id` (consumer-supplied, or default `hash(src, type, dst, origin)`), `src`, `dst`, `type`, `props`, plus the same history and provenance fields as Node |
| ChangeRecord | `graph_version` (u64, monotonic per namespace), `op` (`upsert_node`, `upsert_edge`, `deactivate_node`, `deactivate_edge`), element payload, `idempotency_key?`, `execution_id`, `ts` |
| Snapshot | `snapshot_id`, `namespace`, `graph_version`, `scope` (full \| property filter \| k-hop seeds), part list, node/edge counts, bytes, `created_at`; listed in a versioned manifest |
| GraphSession | `graph_session_id`, `owner` (FR-010), `namespace`, `scope`, `snapshot_version`, `state`, `opened_at`, `last_read_at`, `idle_timeout_s`, `max_lifetime_s`, `resident_bytes`, `instance_id` |
| BackupCheckpoint | `instance_id`, `namespace`, `graph_version` covered, `manifest_version`, object keys, `shipped_at`; lag = head − covered (FR-024 RPO metric) |

Rules:

- Ids are deterministic and supplied by the consumer, which makes upserts idempotent.
- An edge upsert whose `src` or `dst` is unknown or inactive is rejected with
  `INVALID_ARGUMENT`. [PROVISIONAL C21, research.md register: or create placeholder nodes?]
- Props are typed: string, int, float, bool, timestamp, list of scalar, nested map.
  [PROVISIONAL C22, research.md register: is a nested map allowed in the MVP?]
- An idempotency key is remembered per namespace for [PROVISIONAL C23, research.md register: window].

## Durable layout (proposed; research.md R3)

Everything below lives under the instance's `data_dir`, partitioned by namespace. It is
mirrored to `backup_prefix` (`<bucket>/<instance_id>/...`) by the backup shipper.

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
  segments covered by a retained snapshot may be compacted. [PROVISIONAL C02, research.md register FR-003]

## In-memory layout (graph sessions)

- **Element data**: Arrow `RecordBatch`es for nodes and edges.
- **Adjacency**: a CSR adjacency index (out and in) over dense node ordinals, plus an
  id→ordinal hash map. A per-type edge offset is optional.
- **Optional same-node attach**: batches placed in POSIX shm in the `noetl-arrow-cache`
  `IpcHint` format, so colocated processes can attach zero-copy. Cross-node attach is
  never used.
- **Sharing**: read-only, one copy per session (or reference-counted across identical
  `(namespace, scope, version)` — FR-017). Released when the session ends.

## Backup and restore (FR-024)

- **Shipping**:
  - Sealed change-log parts, snapshot files and manifest versions are immutable, so they
    are copied to the object store once (`put_if_absent` semantics).
  - The head pointer is copied last, so a restored instance never points at missing
    objects.
  - Parts seal on size, count **or age** (an EHDB-style `seal_max_age`, enabled), which
    bounds the unshipped tail (research R4).
- **Restore**: into an empty `data_dir`, fetch the manifest and head pointer, then the
  referenced parts and snapshots. Verify the frame CRCs, then rebuild materialized state.
  The instance starts at the backed-up `graph_version`.
- **Replicas (Stage 1, not MVP)**: a follower is a restore plus a change-log tail from a
  cursor (`graph_version`). The same objects and the same rebuild path are used.

## State transitions

- **Instance** (MVP): starting → (restoring →) serving → draining → stopped. If the
  data-dir lock is held by another process, startup fails (FR-025).

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
