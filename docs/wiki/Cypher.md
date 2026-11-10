# Cypher (openCypher, GQL-aligned)

ehgraph accepts a practical subset of **openCypher** for reading and changing a
namespace, alongside the fixed tools. The fixed tools (`graph.upsert`,
`graph.deactivate`, `graph.query`, `graph.neighbors`, `graph.path`, …) are unchanged and
remain the stable contract that existing writers use. Cypher is an extra interface over the same store.

| Interface | Where | Shape |
|---|---|---|
| MCP tool `graph.cypher` | `POST /mcp` `tools/call` | arguments below, envelope result |
| HTTP | `POST /v1/cypher` (= `POST /v1/tools/graph.cypher`) | same body, same envelope |
| Neo4j Query API v2 shape | `POST /db/{namespace}/query/v2`, explicit transactions under `…/query/v2/tx` | `{"statement","parameters"}` → `202 {"data":{"fields","values"},"counters"}` |
| Transactions | `graph.tx.begin` / `run` / `commit` / `rollback` | multi-statement, atomic, serializable. See [Transactions](Transactions) |
| Bolt 5.0 / 4.4 | `[bolt] listen = "127.0.0.1:7687"` (off unless configured) | official Neo4j drivers, `bolt://` |

## 1. The data model it maps onto

ehgraph is a **labeled property graph** (LPG):

| ehgraph | Cypher / GQL |
|---|---|
| namespace | the graph. Chosen by the request (`namespace`, the `/db/{namespace}` path, or Bolt `database=`), never inside the query |
| node `type` (exactly one) | node **label**. `(n:AccessPoint)` matches `type = "AccessPoint"`. A node has exactly one label |
| edge `type` | **relationship type**. Edges are directed |
| `props` (typed: string, int, float, bool, timestamp, list, map) | properties. Timestamps read as `{"$timestamp": rfc3339}` and compare with each other |
| element `id` (caller-chosen string) | the reserved property **`id`**: `n.id`, `{id: 'gw:1'}`, `id(n)`, `elementId(n)`. Never stored in `props`. Matching on `{id: …}` is a direct lookup |
| default edge id `edge:{src}:{type}:{dst}:{origin}` | one relationship per `(start, type, end)` unless the relationship gets an explicit `{id: …}` |
| `active` / soft deactivation | **`DELETE` deactivates**; history and versions are kept. `MATCH` sees active elements (like `graph.query`); pass `"active": false` or `"any"` to read inactive nodes |
| `graph_version` | every result carries it. Reads may pin `version` or a `graph_session_id` |
| `first_seen`, `last_seen`, `source`, `origin`, `execution_id`, `version_updated` | `eh.firstSeen(x)`, `eh.lastSeen(x)`, `eh.source(x)`, `eh.origin(x)`, `eh.executionId(x)`, `eh.version(x)`, `eh.active(x)` |

How writes commit: an updating query runs on a copy-on-write overlay of the head view.
Its net effect (every created or changed node and edge with full property images, plus every
deleted element as a soft deactivation) then commits **atomically as one graph version**,
on condition that nothing the query read changed in the meantime (serializable; a lost race is
re-run on the new head). Multi-statement transactions and conditional (`expect`) commits work the
same way; see [Transactions](Transactions).

Validation, schema rules (`strict`, required properties), idempotency keys and "identical content
does not bump the version" all apply as for `graph.upsert`. So `MERGE … SET` with the same values writes
nothing (`stats.contains_updates = false`, no new version). The `writes` field of the result
describes the commit: `[{"op": "commit", "graph_version", "changed", "applied", "upserted", "deactivated"}]`,
or `[]` when nothing changed.

## 2. Why openCypher

| Option | Fit for ehgraph's LPG | Verdict |
|---|---|---|
| **openCypher** (Cypher Query Language Reference v9, openCypher EBNF + TCK) | Native LPG: labels, typed relationships, properties, pattern matching with variable-length paths, and **updating clauses** (`CREATE`, `MERGE`, `SET`, `DELETE`). The most widely used graph language: Neo4j, Memgraph, AGE, Neptune, FalkorDB, Kùzu | **Chosen.** One language for both reads and the idempotent upserts that ehgraph's writers already do (`MERGE` by id) |
| **ISO/IEC 39075:2024 GQL** | The ISO standard for property-graph queries (April 2024). Its `MATCH`, pattern syntax, `RETURN`, `INSERT`/`SET`/`REMOVE`/`DELETE` come largely from Cypher | **Aligned, not claimed.** The subset uses syntax that is valid openCypher and close to GQL. GQL-only syntax (`INSERT`, `FILTER`, `LET`, quantified path patterns `{m,n}`, `ANY SHORTEST`, path modes, graph types) is on the roadmap |
| SQL/PGQ (ISO/IEC 9075-16:2023) | Graph pattern matching inside SQL over relational tables. Read only | Not a fit for a standalone graph server; worth revisiting with DuckDB/DuckPGQ consumers |
| Gremlin (Apache TinkerPop) | LPG traversal language with an imperative step pipeline, usually over WebSocket or GraphBinary | Possible later as a second front end. Writers would have to change idiom, and LLM agents and people write Cypher far more reliably |
| SPARQL 1.1 (W3C) | RDF triples. No first-class edge properties (needs reification or RDF-star), no labels-as-types, IRIs as identity | **Does not fit**: ehgraph's edges carry properties and types, and ids are plain strings |
| Bolt (Neo4j wire protocol, PackStream) | Transport, not a language. Lets standard drivers send Cypher | **Basic server implemented** (§5) |

This decision revises the MVP register entry C03 ("fixed tools only"). It was provisional and
scoped to the MVP. The fixed tools remain, and Cypher is added alongside them.

References: openCypher project, *Cypher Query Language Reference, Version 9* and the openCypher
grammar/TCK (opencypher.org/resources); ISO/IEC 39075:2024 *Information technology — Database
languages — GQL*; ISO/IEC 9075-16:2023 *SQL/PGQ*; Neo4j *Bolt Protocol* and *PackStream*
specifications (neo4j.com/docs/bolt); Neo4j *Query API* (neo4j.com/docs/query-api); W3C *SPARQL
1.1 Query Language* (2013); Apache TinkerPop *Gremlin* reference.

## 3. Request and result

```json
POST /v1/cypher
{
  "namespace": "qnetmon",
  "query": "MATCH p = (a:AccessPoint)-[:UPLINK*1..4]->(g:Gateway) RETURN a.name AS ap, length(p) AS hops",
  "params": {},
  "active": true,              // optional: true (default) | false | "any" — read-only queries only
  "version": 4569,             // optional, read-only: pin a version
  "graph_session_id": "…",     // optional, read-only: read a session graph
  "source": "cypher",          // optional: `source` stamped on written elements (default "cypher")
  "execution_id": "…",         // optional (default "cypher:<ms>")
  "idempotency_key": "…",      // optional: passed to graph.upsert (and "<key>:deactivate")
  "max_rows": 100000,          // optional cap on intermediate/result rows (≤ 1,000,000)
  "expect": {"versions": {"gw:1": 9121}, "graph_version": 9121}  // optional, updating only: see Transactions
}
```

```json
{"ok": true, "graph_version": 4569, "data": {
  "columns": ["ap", "hops"],
  "rows": [["U7 Pro", 2]],
  "stats": {"contains_updates": false, "nodes_created": 0, "nodes_deleted": 0,
            "relationships_created": 0, "relationships_deleted": 0, "properties_set": 0},
  "writes": []}}
```

Nodes come back as `{"elementId","labels","properties"}`, relationships as
`{"elementId","type","startNodeElementId","endNodeElementId","properties"}`, and paths as
`{"nodes":[…],"relationships":[…]}`. These are the Neo4j Query API's plain-JSON shapes.

Errors use the normal envelope: `INVALID_ARGUMENT` (400) for syntax, type, semantic and
unsupported-feature errors, with messages that start `cypher syntax error: …`,
`cypher unsupported error: …` and so on. `CONFLICT` (409) is for constraint errors (a `CREATE` of an
existing id, or a `DELETE` of a node that still has relationships). `VERSION_CONFLICT` (409) means
a failed `expect` or a serialization conflict that persisted after the automatic retries. Nothing was written. `UNAUTHORIZED` (401) is for a
write without a write grant, and `READ_ONLY_SESSION` (403) for a write with `version` or `graph_session_id`.

## 4. Supported subset and gaps

### Clauses

| Feature | Status | Notes |
|---|---|---|
| `MATCH`, multiple comma-separated patterns, `WHERE` | ✅ | Relationship uniqueness within a `MATCH` (no edge bound twice) |
| `OPTIONAL MATCH` | ✅ | |
| Node patterns `(n:Label {k: v})`, `$param` property maps | ✅ | `{id: …}` is an index lookup; otherwise a label scan |
| Relationship patterns `-[r:T]->`, `<-[r]-`, `-[r]-`, `:A\|B` | ✅ | |
| Variable length `*`, `*n`, `*m..n`, `*..n`, `*m..`, `*0..` | ✅ | Trail semantics. Upper bound is capped at 16 hops (larger explicit bounds are rejected). Expansion has a work budget |
| Named paths `p = (…)` | ✅ | `nodes(p)`, `relationships(p)`, `length(p)` |
| `shortestPath(…)`, `allShortestPaths(…)` | ✅ | One relationship pattern, min length 0 or 1 |
| `WITH` (projection, `DISTINCT`, `ORDER BY`, `SKIP`, `LIMIT`, `WHERE`) | ✅ | |
| `UNWIND` | ✅ | The batch-upsert idiom `UNWIND $rows AS r MERGE …` |
| `RETURN` (`*`, aliases, `DISTINCT`, `ORDER BY`, `SKIP`, `LIMIT`) | ✅ | |
| Aggregation `count(*)`, `count/collect/sum/avg/min/max` (with `DISTINCT`) | ✅ | Implicit grouping keys, as in openCypher |
| `CREATE` nodes, relationships, paths | ✅ | Exactly one label. Missing `id` is generated (`<label>:<hex>`). Existing active id → 409 (use `MERGE`) |
| `MERGE` (`ON CREATE SET`, `ON MATCH SET`) | ✅ | Matches the whole pattern, else creates it. Reactivates a deactivated id |
| `SET n.k = v`, `SET n = map`, `SET n += map`, `SET n:Label` (same label only) | ✅ / ⚠️ | `null` removes. `id` is immutable. A second label is out of model |
| `REMOVE n.k` | ✅ | `REMOVE n:Label` ❌ (single-label model) |
| `DELETE`, `DETACH DELETE` | ✅ | **Soft** deactivation. Dangling check at end of query, as in Neo4j |
| `UNION` / `UNION ALL` | ❌ TODO | |
| `CALL` procedures, `CALL { subquery }`, `EXISTS { }`, `COUNT { }` | ❌ TODO | |
| Pattern predicates in `WHERE` (`WHERE (a)-->(b)`) and pattern comprehensions | ❌ TODO | Use `OPTIONAL MATCH` + `count` |
| `FOREACH`, `LOAD CSV` | ❌ | Not planned (`UNWIND` covers batch writes) |
| Multiple labels per node, label expressions `:A&B`, `:!A` | ❌ | Model has one type per node |
| Parallel relationships of the same type between the same nodes | ⚠️ | Only with an explicit relationship `{id: …}` |
| Schema DDL (`CREATE INDEX/CONSTRAINT`) | ❌ | Schema is `data_dir/schemas/<ns>.json` |
| Multi-statement transactions | ✅ | `graph.tx.*`, Bolt BEGIN/COMMIT/ROLLBACK, Query API `/tx`. Snapshot reads + read-your-writes; one atomic, serializable commit (optimistic, first committer wins); conditional commits with `expect`. See [Transactions](Transactions) |

### Expressions and functions

✅ literals (int, float, string, bool, null, list, map), `$params`, property access (nested
maps too), `[i]`, `[a..b]`, `+ - * / % ^`, string `+`, list `+`, `= <> < <= > >=`, three-valued
`AND OR XOR NOT`, `IS [NOT] NULL`, `IN`, `STARTS WITH`, `ENDS WITH`, `CONTAINS`, `n:Label` tests,
`CASE` (simple and searched), list comprehensions `[x IN l WHERE p | e]`, and `all/any/none/single(x IN l WHERE p)`.

Functions: `id`, `elementId`, `labels`, `type`, `properties`, `keys`, `startNode`, `endNode`,
`nodes`, `relationships`, `length`, `size`, `coalesce`, `exists`, `head`, `last`, `tail`,
`reverse`, `range`, `toLower`, `toUpper`, `trim`, `lTrim`, `rTrim`, `replace`, `split`,
`substring`, `left`, `right`, `toString`, `toInteger`, `toFloat`, `toBoolean`, `abs`, `ceil`,
`floor`, `round`, `sqrt`, `sign`, `timestamp`, and `eh.*` metadata.

❌ TODO: regular expressions (`=~`), temporal types and functions (`date()`, `datetime()`,
`duration`; ehgraph timestamps are exposed as `$timestamp` values), spatial types, `reduce()`,
string functions beyond the list above, and trigonometric and log functions.

### Semantics differences to know

- `id(n)` returns the ehgraph **string** id, not Neo4j's integer.
- `properties_set` counts properties that actually changed (Neo4j counts every `SET`), so an idempotent rerun reports 0.
- A node that already has a property literally named `id` in `props` (written through `graph.upsert`) shows its element id through `n.id`. The prop is still visible via `properties(n)`.
- Traversal follows active edges between active nodes. `active: false` affects which nodes `MATCH` can bind. It does not resurrect deactivated edges.

## 5. Bolt

Configure `[bolt]` in the standalone server's TOML. It is off unless the section is present:

```toml
[bolt]
listen = "127.0.0.1:7687"     # plaintext: loopback, or put TLS in front
default_namespace = "qnetmon" # used when the client names no database
```

```python
from neo4j import GraphDatabase                    # official driver, tested with 5.28
with GraphDatabase.driver("bolt://127.0.0.1:7687", auth=("any", "<ehgraph bearer token>")) as d:
    with d.session(database="qnetmon") as s:      # database = namespace
        for r in s.run("MATCH (a:AccessPoint)-[:UPLINK*]->(g:Gateway) RETURN a.name, g.name"):
            print(r)
```

| Bolt feature | Status |
|---|---|
| Handshake: 5.0 preferred, 4.4 fallback | ✅ 5.0 sends `element_id` = ehgraph id. Under 4.4 the integer ids are a stable hash |
| HELLO (basic/bearer → ehgraph token), LOGON/LOGOFF, RESET, GOODBYE, TELEMETRY | ✅ |
| RUN / PULL (`n`, `has_more`) / DISCARD; FAILURE → IGNORED until RESET | ✅ |
| BEGIN / COMMIT / ROLLBACK, `execute_read` / `execute_write` | ✅ Real transactions: RUNs read the snapshot plus their own writes. COMMIT writes everything as one version, or fails with `Neo.TransientError.Transaction.Outdated` on a lost race (the drivers retry). ROLLBACK, RESET or a dropped connection discards. `tx_timeout` and `tx_metadata.expect` are honoured |
| Node, Relationship, Path, UnboundRelationship structs | ✅ |
| Server agent | `Neo4j/5.0.0 compatible graph database server - ehgraph …` (drivers refuse any agent without the `Neo4j/` prefix; Memgraph does the same) |
| ROUTE / `neo4j://` routing | ❌ Use `bolt://` |
| TLS on the Bolt listener | ❌ TARGET |
| Bolt 5.1+ (LOGON-only auth), 5.x notifications, temporal/spatial structs as parameters | ❌ TARGET |
| Embedded mode (host-owned Bolt listener) | ⚠️ `ehgraph::bolt::serve(graph, listener, options)` is public. Hosts must bind it themselves |

## 6. Examples (qnetmon topology)

```cypher
// Which devices uplink directly to the gateway?
MATCH (d)-[:UPLINK]->(g:Gateway) RETURN labels(d)[0] AS kind, d.name, g.name

// Every AP's path to the internet
MATCH p = shortestPath((a:AccessPoint)-[*..8]->(:Internet))
RETURN a.name, [n IN nodes(p) | n.name] AS via

// Clients per AP, including APs with none
MATCH (a:AccessPoint) OPTIONAL MATCH (c:Client)-[:CONNECTED_TO]->(a)
RETURN a.name, count(c) AS clients ORDER BY clients DESC

// Offline view (request "active": false)
MATCH (n) RETURN labels(n)[0] AS kind, n.id, eh.lastSeen(n) ORDER BY kind

// Idempotent topology upsert from a playbook (params.rows = [{id, type_props…}])
UNWIND $rows AS r
MERGE (n:Device {id: r.id}) SET n += r.props
```

## 7. Roadmap to fuller GQL / Bolt

1. `UNION`, `EXISTS {}` / `COUNT {}` subqueries, pattern predicates, `=~`, `reduce()`.
2. GQL surface syntax alongside the Cypher forms: `INSERT`, `FILTER`, `LET`, quantified path patterns `((a)-[:T]->(b)){1,4}`, `ANY SHORTEST` / `ALL SHORTEST`, path modes (`TRAIL`, `ACYCLIC`).
3. Run the openCypher TCK scenarios that fall within the single-label model, and publish the pass rate.
4. ~~Conditional commit and multi-statement Bolt transactions~~: done, see [Transactions](Transactions). Next: property-granular conflict detection and server-side retry of explicit transactions.
5. Bolt: TLS, 5.1+ (LOGON), and a routing table for single-instance `neo4j://`.
6. Temporal types mapped to ehgraph timestamps, and secondary property indexes for large label scans.
