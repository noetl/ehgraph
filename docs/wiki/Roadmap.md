# Roadmap

## Stage 0 — this release

One primary per domain.

- Durable change log and snapshots.
- Age-bounded seal and backup/restore through a filesystem prefix.
- Session-pinned memory.
- MCP and HTTP.
- Single writer: StatefulSet `replicas: 1`, RWO volume, data-directory lock.

RPO is the reported backup lag. RTO is however long the copy and rebuild take. That is accepted for the MVP.

## Stage 1 — not built

A read replica would tail the change log from a `graph_version` cursor and serve reads and sessions. Writes would be rejected with `UNAVAILABLE` and a hint pointing at the primary. The tool names stay the same.

A warm standby would be that follower plus a manual, owner-gated promotion. Promotion needs a Kubernetes Lease election and store-side fencing: the store refuses an append whose epoch is below the highest epoch it has accepted. EHDB's election and fencing code is a design reference and is not authoritative there either. ehgraph will not enable `role = replica` or `role = standby` until both exist. Today the server exits if you set either role.

The trigger to start Stage 1 is a measured one: an RTO tighter than restoring a volume, or read and session load that does not fit on one node. That measurement is not taken yet.

The storage boundary that makes Stage 1 possible without a rewrite is already there. Each namespace log is an ordered sequence of frames keyed by `graph_version`. Sealed parts are immutable. A follower is a restore plus a tail from a cursor. No code in this release subscribes to that tail.

## Deferred — sharding and multi-writer

Topology traversals (k-hop, shortest path) stay on one node because a hop that crosses a partition becomes a network round trip, and these graphs partition badly. The graphs this server is for are assumed to fit in one node's memory, up to about 10^6 elements. Scale out by adding instances (another domain, or another purpose), not by splitting one namespace. Multi-writer would break the single ordered log.

Revisit sharding only with a measured domain that does not fit on one machine.

## Not in this release

- Full openCypher / GQL. A practical openCypher subset (reads, `CREATE`/`MERGE`/`SET`/`DELETE`, variable-length paths), the Neo4j Query API shape and a basic Bolt listener shipped on 2026-11-10. The fixed tools remain the stable API. Gap table and next steps: [Cypher](Cypher).
- A NoETL `kind: graph` tool. Consumers use `kind: mcp`.
- Session-local what-if edits.
- Sharing one memory copy across sessions.
- POSIX shared memory in the `noetl-arrow-cache` `IpcHint` format. Remote readers use HTTP. Same-node attach can be added later without changing the session model.
- Compaction of log segments covered by a retained snapshot. Snapshots are kept (default 16). The log is not trimmed.
- An S3 or GCS client. `backup_prefix` is a directory. The shipper's rule (immutable put-if-absent, head last) is the same rule a later object client must keep.
