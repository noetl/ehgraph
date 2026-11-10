# Transactions and conditional commits

ehgraph has three kinds of guarded write. Existing writers that send plain `graph.upsert` /
`graph.deactivate` without `expect` are unchanged: the same code path, the same versions, the same responses.

| What | Where | Guarantee |
|---|---|---|
| **Conditional commit** (`expect`) | `graph.upsert`, `graph.deactivate` (by `ids`), `graph.cypher`, `graph.tx.commit`, Query API commit, Bolt `BEGIN {tx_metadata: {expect}}` | Applies only if every expected element version (and/or the graph version) still holds. Otherwise `VERSION_CONFLICT` (409) and nothing is written |
| **Auto-commit Cypher write** | `graph.cypher`, Query API `POST /db/{ns}/query/v2`, Bolt RUN outside BEGIN | Atomic (one graph version for all upserts **and** deactivations), serializable. A lost race is re-run on the new head up to 3 times, then `VERSION_CONFLICT` |
| **Multi-statement transaction** | `graph.tx.begin/run/commit/rollback`, Query API `/tx`, Bolt BEGIN/RUN/COMMIT/ROLLBACK | Snapshot reads plus read-your-writes. All statements commit atomically as one graph version, serializably, or not at all |

## Conditional commits: `expect`

```json
"expect": {
  "graph_version": 9121,                 // optional: namespace head must equal this (0 = namespace absent)
  "versions": {"gw:1": 9121,             // element's version_updated must equal this
               "lock:job-7": 0}          // 0 = element must not exist (create-if-absent)
}
```

* Element versions are `version_updated`, which every element already carries (`graph.node`,
  `graph.edge`, `eh.version(n)` in Cypher). A deactivation bumps it, so a concurrent delete fails a stale expectation.
* Expectations are checked under the store's write lock, in the same critical section that appends
  the commit, so there is no window between the check and the write.
* An unknown key inside `expect` is rejected (400), never ignored. Servers before this feature ignore an
  unknown top-level `expect`. Check `graph.instance.info` → `features` contains `"expect"` before you rely on it.

Typical optimistic-locking loop:

```python
n = call("graph.node", {"namespace": ns, "id": "gw:1"})["data"]
call("graph.upsert", {"namespace": ns, "source": "ops", "execution_id": "e1",
     "nodes": [{"id": "gw:1", "type": "Gateway", "props": {**n["props"], "maintenance": True}}],
     "expect": {"versions": {"gw:1": n["version_updated"]}}})   # 409 → re-read and retry
```

## Multi-statement transactions

```text
graph.tx.begin    {namespace, timeout_ms?, source?, execution_id?} → {tx_id, graph_version (snapshot), expires_ms}
graph.tx.run      {tx_id, query, params?, max_rows?}               → {columns, rows, stats, expires_ms}
graph.tx.commit   {tx_id, expect?, idempotency_key?}               → {graph_version, changed, writes}
graph.tx.rollback {tx_id}                                          → {rolled_back}
```

Neo4j Query API shape (same semantics):

| Request | Effect |
|---|---|
| `POST /db/{ns}/query/v2/tx` (optional `{"statement","parameters"}`) | open; `202 {"transaction": {"id","expires"}, "data"?, "counters"?}` |
| `POST /db/{ns}/query/v2/tx/{id}` `{"statement","parameters"}` | run a statement in the transaction |
| `POST /db/{ns}/query/v2/tx/{id}/commit` (optional statement, ehgraph extension `expect`) | commit; `202 {"bookmarks": ["ehgraph:{ns}:{version}"]}` |
| `DELETE /db/{ns}/query/v2/tx/{id}` | roll back (404 `TransactionNotFound` if it is already gone) |

Bolt: BEGIN opens a transaction (database = namespace, `tx_timeout` honoured, optional
`tx_metadata.expect`). RUN and PULL run inside it. COMMIT returns a bookmark. ROLLBACK, RESET,
GOODBYE and a dropped connection all discard it. `session.execute_write(fn)` in the official drivers
works as is, including retry on conflict (verified with the Python driver 5.28.1: 6 threads ×
15 read-modify-write increments ended at exactly 90, with the conflicts retried by the driver).

### Semantics

* **Snapshot.** BEGIN pins the namespace head view. Every statement reads that snapshot plus the
  transaction's own earlier writes. Other writers' commits are not visible inside the transaction.
* **Nothing is written before COMMIT.** Writes live in the executor's copy-on-write overlay, carried
  from statement to statement. ROLLBACK frees memory. It never touches the store.
* **A failed statement fails the transaction** (as in Neo4j): later statements and COMMIT are refused,
  and only ROLLBACK is accepted. Earlier writes are discarded with it.
* **COMMIT is atomic.** The net effect (upserts with full property images plus soft deactivations)
  goes to `Store::transact` and is appended as **one** change-log frame, so one `graph_version`.
  Validation, schema rules and "identical content does not bump the version" all apply as for
  `graph.upsert`. A transaction whose net effect is empty commits without a new version.
* **COMMIT is serializable** (optimistic concurrency control, first committer wins). While it
  executes, a transaction records a **footprint**:

  | Recorded | When | A later commit conflicts if it… |
  |---|---|---|
  | element ids | any lookup (found or not), every element written | changed that element |
  | labels | a label scan `MATCH (n:L)` | created or changed any `L` node (phantoms) |
  | "all nodes" | a label-less scan `MATCH (n)` | changed any node |
  | adjacency of node *x* (out / in) | an expansion from *x* | created, changed or removed an edge at *x* |

  Under the write lock, COMMIT scans the elements with `version_updated > snapshot`. If none
  intersects the footprint, the transaction's reads are still exactly what the head would return,
  so committing now is equivalent to running the transaction serially at the head. If one does, the
  commit fails with `VERSION_CONFLICT` and a message that starts `serialization conflict:` and
  names the element. Nothing is written, and the client re-runs the transaction. If the head has not
  moved, the scan is skipped.
* **Granularity.** Validation is per element, label and adjacency. It is not per property, so a
  concurrent write to a *different* property of a node you read still conflicts. Writes to elements
  you neither read nor scanned do not (e.g. the qnetmon topology writer updating APs while a
  transaction edits `Account` nodes).
* **Timeouts and limits.** Idle timeout is 30 s by default and at most 10 min (`timeout_ms`, Bolt
  `tx_timeout`). Each statement resets it. At most 256 transactions are open per instance. A
  transaction belongs to the principal that opened it. BEGIN checks the read grant, and every
  updating statement and a COMMIT that writes check the write grant. An open transaction keeps its snapshot view resident until it ends: keep them short.

### Error mapping

| ehgraph | HTTP | Neo4j code (Bolt, Query API) | Retry? |
|---|---|---|---|
| `VERSION_CONFLICT` "serialization conflict: …" | 409 | `Neo.TransientError.Transaction.Outdated` | yes: re-run the whole transaction (drivers do this in `execute_write`) |
| `VERSION_CONFLICT` "expectation failed: …" | 409 | `Neo.ClientError.Schema.ConstraintValidationFailed` | after re-reading the versions you expect |
| `NOT_FOUND` "transaction … not found" | 404 | `Neo.ClientError.Transaction.TransactionNotFound` | no (expired, committed or rolled back) |

## Design notes

* **Why OCC, not locks.** ehgraph already serializes commits through one store lock and keeps a
  per-element `version_updated`. Validating a read footprint against "what changed since my
  snapshot" needs no lock table, cannot deadlock, and costs nothing for the existing
  single-request writers. Long transactions pay with possible conflicts, not by blocking writers.
* **No on-disk format change.** A change-log frame could always hold a mix of upsert and
  deactivate ops. Transactions just use that, so older builds read the log unchanged.
* **Auto-commit Cypher changed shape.** It used to commit upserts and deactivations as two
  versions (`writes: [{op: "graph.upsert"}, {op: "graph.deactivate"}]`). It now commits one:
  `writes: [{"op": "commit", "graph_version", "changed", "applied", "upserted", "deactivated"}]`.
* Code: `crates/ehgraph-store` (`Store::transact`, `Expect` checks), `crates/ehgraph-cypher`
  (`execute_in`, `TxState`, `Footprint`), `crates/ehgraph-api/src/tx.rs` (registry, commit,
  tools), `crates/ehgraph/src/bolt.rs`, `crates/ehgraph/src/http.rs`. Tests:
  `crates/ehgraph-store/tests/transact.rs`, `crates/ehgraph-server/tests/transactions.rs` (atomicity,
  rollback, isolation, lost update, phantom, adjacency write skew, concurrent increments, `expect`,
  Query API), `crates/ehgraph-server/tests/bolt.rs`.
