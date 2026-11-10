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
| `WITH` (projection, `DISTINCT`, `ORDER BY`, `SKIP`, `LIMIT`, `WHERE`) | ✅ | `WITH … WHERE` also sees the variables in scope before the projection, as in openCypher |
| `UNWIND` | ✅ | The batch-upsert idiom `UNWIND $rows AS r MERGE …` |
| `RETURN` (`*`, aliases, `DISTINCT`, `ORDER BY`, `SKIP`, `LIMIT`) | ✅ | `RETURN *` columns are the variables in scope, sorted by name (also when there are no rows) |
| Aggregation `count(*)`, `count/collect/sum/avg/min/max` (with `DISTINCT`) | ✅ | Implicit grouping keys, as in openCypher |
| `CREATE` nodes, relationships, paths | ✅ | Exactly one label. Missing `id` is generated (`<label>:<hex>`). Existing active id → 409 (use `MERGE`) |
| Nodes without a label (`CREATE ()`, `MERGE ({k: v})`) | ❌ | Every ehgraph node has a type. This is the largest single cause of TCK failures (§8): 391 scenarios create an unlabeled node in their setup or query |
| `MERGE` (`ON CREATE SET`, `ON MATCH SET`) | ✅ | Matches the whole pattern, else creates it. Reactivates a deactivated id |
| `SET n.k = v`, `SET n = map`, `SET n += map`, `SET n:Label` (same label only) | ✅ / ⚠️ | `null` removes. `id` is immutable. A second label is out of model |
| `REMOVE n.k` | ✅ | `REMOVE n:Label` ❌ (single-label model) |
| `DELETE`, `DETACH DELETE` | ✅ | **Soft** deactivation. Dangling check at end of query, as in Neo4j |
| `UNION` / `UNION ALL` | ✅ | Same column names required; `UNION` removes duplicate rows; mixing the two is a syntax error |
| `EXISTS { [MATCH] pattern [WHERE …] }` | ✅ | Full subqueries inside `EXISTS { }` (`WITH`, `RETURN`, nested `MATCH` clauses) ❌ TODO |
| `CALL` procedures, `CALL { subquery }`, `COUNT { }` | ❌ TODO | |
| Pattern predicates (`WHERE (a)-[:T]->(b)`, `exists((a)-->())`) and pattern comprehensions `[(a)-->(b) WHERE … \| b.x]` | ✅ | A bare pattern is a predicate: only in `WHERE` (under `AND`/`OR`/`XOR`/`NOT`) or in `exists()`, and it cannot introduce new variables |
| Compile-time checks | ✅ | Undefined variables, a variable reused as another kind (node / relationship / path / value), rebinding in `CREATE`/`MERGE`, a bound path variable, aggregates outside projections or nested, aggregate `ORDER BY` without an aggregating projection, and non-projected variables in `ORDER BY` after `DISTINCT` or aggregation. Reported as `cypher semantic error` (Neo4j: `SyntaxError`) before anything runs |
| `FOREACH`, `LOAD CSV` | ❌ | Not planned (`UNWIND` covers batch writes) |
| Multiple labels per node, label expressions `:A&B`, `:!A` | ❌ | Model has one type per node |
| Parallel relationships of the same type between the same nodes | ⚠️ | Only with an explicit relationship `{id: …}` |
| Mixed-direction relationship `<-[]->` | ❌ | A syntax error in ehgraph; the TCK reads it as undirected (3 scenarios) |
| Schema DDL (`CREATE INDEX/CONSTRAINT`) | ❌ | Schema is `data_dir/schemas/<ns>.json` |
| Multi-statement transactions | ✅ | `graph.tx.*`, Bolt BEGIN/COMMIT/ROLLBACK, Query API `/tx`. Snapshot reads + read-your-writes; one atomic, serializable commit (optimistic, first committer wins); conditional commits with `expect`. See [Transactions](Transactions) |

### Expressions and functions

✅ literals (int, float, string, bool, null, list, map; hexadecimal `0x1F`, octal `0o17`, `.5`,
`-9223372036854775808`), `$params`, property access (nested maps too), `[i]`, `[a..b]`,
`+ - * / % ^` (`^` is left-associative), string `+`, list `+`, `= <> < <= > >=` (chained:
`a < b < c` is `a < b AND b < c`; comparisons with `NaN` are `false`), three-valued
`AND OR XOR NOT`, `IS [NOT] NULL`, `IN`, `STARTS WITH`, `ENDS WITH`, `CONTAINS`, `n:Label` tests,
`CASE` (simple and searched), list comprehensions `[x IN l WHERE p | e]`, and `all/any/none/single(x IN l WHERE p)`.

Functions: `id`, `elementId`, `labels`, `type`, `properties`, `keys`, `startNode`, `endNode`,
`nodes`, `relationships`, `length`, `size`, `coalesce`, `exists`, `head`, `last`, `tail`,
`reverse`, `range`, `toLower`, `toUpper`, `trim`, `lTrim`, `rTrim`, `replace`, `split`,
`substring`, `left`, `right`, `toString`, `toInteger`, `toFloat`, `toBoolean`, `abs`, `ceil`,
`floor`, `round`, `sqrt`, `sign`, `rand`, `timestamp`, and `eh.*` metadata.

❌ TODO: regular expressions (`=~`), temporal types and functions (`date()`, `datetime()`,
`duration`; ehgraph timestamps are exposed as `$timestamp` values), spatial types, `reduce()`,
`percentileCont`/`percentileDisc`/`stDev`, string functions beyond the list above, and
trigonometric and log functions. (The TCK at the pinned commit does not exercise `=~`, `reduce()`
or the trigonometric functions; temporal types alone are 1,069 of its scenarios.)

### Semantics differences to know

- `id(n)` returns the ehgraph **string** id, not Neo4j's integer.
- `id` is the element id, not a property: `CREATE ({id: 1})` sets the element id `"1"`, so `n.id` reads back the string `'1'` and `properties(n)` has no `id`. TCK scenarios that use a property named `id` fail for this reason (§8).
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

1. ~~`UNION`, pattern predicates~~: done. Full `EXISTS {}` / `COUNT {}` / `CALL {}` subqueries, `=~`, `reduce()`.
2. GQL surface syntax alongside the Cypher forms: `INSERT`, `FILTER`, `LET`, quantified path patterns `((a)-[:T]->(b)){1,4}`, `ANY SHORTEST` / `ALL SHORTEST`, path modes (`TRAIL`, `ACYCLIC`).
3. ~~Run the openCypher TCK and publish the pass rate~~: done, §8. Next: decide whether unlabeled nodes get a model mapping (the largest TCK failure cause).
4. ~~Conditional commit and multi-statement Bolt transactions~~: done, see [Transactions](Transactions). Next: property-granular conflict detection and server-side retry of explicit transactions.
5. Bolt: TLS, 5.1+ (LOGON), and a routing table for single-instance `neo4j://`.
6. Temporal types mapped to ehgraph timestamps, and secondary property indexes for large label scans.

## 8. TCK conformance

Run on 2026-11-10 against the openCypher TCK **2024.3**, commit
[`677cbafabb8c3c5eed458fd3b1ec0daec8d67d23`](https://github.com/opencypher/openCypher/tree/677cbafabb8c3c5eed458fd3b1ec0daec8d67d23/tck/features)
(Apache-2.0; the feature files are not vendored here). The scenario-level results are in
[`tck-results.json`](tck-results.json).

### Results

| | Scenarios | Pass | Fail | Out of model | Pass rate, all scenarios | Pass rate, in-model |
|---|---:|---:|---:|---:|---:|---:|
| Before the fixes in this change (main at `9d97955`) | 3,896 | 1,844 | 882 | 1,170 | 47.3% | 67.6% |
| After | 3,896 | **2,292** | 434 | 1,170 | **58.8%** | **84.1%** |

"In-model" is pass / (pass + fail). One more scenario is tagged `@ignore` by the TCK itself and is
not run. 391 of the 434 remaining failures are scenarios that create a node without a label
(see the top causes below); without them the in-model pass rate would be 98.2%, but they are
counted as failures because an unlabeled node is a gap, not a reason to exclude a scenario.

### Reproduce

```bash
scripts/tck.sh [workdir]          # clones openCypher at the pinned commit, runs the ignored test
EHGRAPH_TCK_FILTER=clauses/match scripts/tck.sh   # one directory
```

The runner is `crates/ehgraph/tests/tck.rs` (an `#[ignore]` test, so `cargo test --workspace`
does not need the checkout). It drives the real path end to end: an in-process instance on a temp
directory, `EhGraph::cypher_as` → `Engine::run_cypher` → the store commit, one fresh namespace per
scenario. It writes `target/tck-results.json` (or `$EHGRAPH_TCK_OUT`).

### How results are compared

- **Values.** Nodes compare by label set and properties, relationships by type and properties,
  paths by their sequence of nodes and relationships and each relationship's direction. ehgraph
  element ids are identity, not properties, and are never compared. Integers and floats are
  distinct (`1` ≠ `1.0`); floats compare exactly, `NaN` equals `NaN`. Column names must match.
  `in any order` compares rows as a multiset; `ignoring element order for lists` compares lists
  as multisets.
- **Side effects** are a diff of the visible graph before and after the query, as the TCK defines
  them: `±nodes` and `±relationships` by element id, `±properties` by (element, key, value),
  `±labels` by the set of label names present in the graph. They are not taken from ehgraph's
  `stats` (which count changed properties only). A soft-deleted element is gone from the visible
  graph, so it counts as deleted.
- **Empty graph.** Reads on a namespace that was never written are `NOT_FOUND` in ehgraph, and an
  empty upsert is refused, so each scenario's namespace is created by upserting one sentinel node
  and deactivating it. Deactivated elements are invisible to `MATCH`.
- **Expected errors** pass when ehgraph raises an error of a mapped kind. The phase
  (compile time or runtime) and the detail code are not checked:

  | TCK error | Accepted ehgraph kinds |
  |---|---|
  | `SyntaxError` | `syntax`, `unsupported`, `semantic` (the TCK reports semantic analysis errors such as `UndefinedVariable` as `SyntaxError`); also `type` when the detail is `InvalidArgumentType` (a type check ehgraph makes at runtime) |
  | `SemanticError` | `semantic`, `syntax` |
  | `TypeError` | `type` |
  | `ArgumentError` | `type`, `semantic`, `limit` |
  | `ConstraintVerificationFailed` | `constraint` |
  | `EntityNotFound` | `semantic`, `type`, `constraint` |
  | `ParameterMissing` | `semantic`, `syntax` |

  An `unsupported` error where the scenario expects success is a failure.

### Out-of-model rules

A scenario is excluded only when it needs something outside ehgraph's model. Each excluded
scenario's rule is recorded in `tck-results.json`.

| Rule | How it is detected | Scenarios |
|---|---|---:|
| Temporal and spatial types | A query calls `date`, `datetime`, `localdatetime`, `time`, `localtime`, `duration`, `point` or `distance` (also `datetime.truncate(…)` style) outside a string | 1,069 |
| Procedures (`CALL proc`) | `CALL name…` in a query, or a `there exists a procedure` step. `CALL { subquery }` is not excluded | 52 |
| Multiple labels per node | A node pattern or label test with two or more labels or a label expression (`:A:B`, `:A\|B`, `:A&B` on a node) in a query or in an expected row; or ehgraph rejects a statement for adding a second label | 44 |
| Parallel relationships of one type | ehgraph rejects a `CREATE` because the same `(start, type, end)` relationship already exists (no explicit relationship id) | 5 |
| Schema DDL | `CREATE`/`DROP` `INDEX`/`CONSTRAINT` | 0 |

Not excluded, counted as failures: nodes without a label, properties named `id` (ehgraph's
element id), `<-[]->`, row limits.

### By directory

| Directory | Scenarios | Pass | Fail | Out of model | In-model pass rate | Before |
|---|---:|---:|---:|---:|---:|---:|
| `clauses/call` | 52 | 0 | 0 | 52 | – | – |
| `clauses/create` | 78 | 35 | 40 | 3 | 47% | 41% |
| `clauses/delete` | 41 | 18 | 23 | 0 | 44% | 41% |
| `clauses/match` | 381 | 332 | 44 | 5 | 88% | 26% |
| `clauses/match-where` | 34 | 21 | 13 | 0 | 62% | 59% |
| `clauses/merge` | 75 | 47 | 21 | 7 | 69% | 66% |
| `clauses/remove` | 33 | 10 | 11 | 12 | 48% | 48% |
| `clauses/return` | 63 | 19 | 44 | 0 | 30% | 27% |
| `clauses/return-orderby` | 35 | 21 | 14 | 0 | 60% | 57% |
| `clauses/return-skip-limit` | 31 | 20 | 11 | 0 | 65% | 65% |
| `clauses/set` | 53 | 27 | 16 | 10 | 63% | 58% |
| `clauses/union` | 12 | 12 | 0 | 0 | 100% | 33% |
| `clauses/unwind` | 14 | 11 | 3 | 0 | 79% | 79% |
| `clauses/with` | 29 | 14 | 15 | 0 | 48% | 38% |
| `clauses/with-orderBy` | 292 | 188 | 39 | 65 | 83% | 71% |
| `clauses/with-skip-limit` | 9 | 1 | 8 | 0 | 11% | 11% |
| `clauses/with-where` | 19 | 7 | 12 | 0 | 37% | 37% |
| `expressions/aggregation` | 35 | 16 | 19 | 0 | 46% | 46% |
| `expressions/boolean` | 150 | 149 | 1 | 0 | 99% | 99% |
| `expressions/comparison` | 72 | 52 | 20 | 0 | 72% | 65% |
| `expressions/conditional` | 13 | 12 | 1 | 0 | 92% | 92% |
| `expressions/existentialSubqueries` | 10 | 4 | 6 | 0 | 40% | 10% |
| `expressions/graph` | 60 | 17 | 31 | 12 | 35% | 35% |
| `expressions/list` | 185 | 180 | 5 | 0 | 97% | 97% |
| `expressions/literals` | 131 | 131 | 0 | 0 | 100% | 69% |
| `expressions/map` | 44 | 44 | 0 | 0 | 100% | 100% |
| `expressions/mathematical` | 6 | 5 | 1 | 0 | 83% | 83% |
| `expressions/null` | 44 | 40 | 4 | 0 | 91% | 91% |
| `expressions/path` | 7 | 5 | 2 | 0 | 71% | 71% |
| `expressions/pattern` | 50 | 43 | 7 | 0 | 86% | 34% |
| `expressions/precedence` | 121 | 121 | 0 | 0 | 100% | 96% |
| `expressions/quantifier` | 604 | 604 | 0 | 0 | 100% | 90% |
| `expressions/string` | 32 | 32 | 0 | 0 | 100% | 100% |
| `expressions/temporal` | 1,004 | 0 | 0 | 1,004 | – | – |
| `expressions/typeConversion` | 47 | 25 | 22 | 0 | 53% | 53% |
| `useCases/countingSubgraphMatches` | 11 | 10 | 1 | 0 | 91% | 91% |
| `useCases/triadicSelection` | 19 | 19 | 0 | 0 | 100% | 5% |

### Top remaining failure causes

| Cause | Scenarios |
|---|---:|
| A node without a label (`CREATE ()`, `CREATE (:A)-[:T]->()`, `MERGE ({k: v})`) in the setup or the query | 391 |
| A property named `id`: it becomes the element id, so `n.id` returns a string, `properties(n)` lacks it, and two nodes cannot share an `id` value | 18 (plus 33 that also create unlabeled nodes) |
| Compile-time checks not done: `InvalidArgumentType` on literals (7), `AmbiguousAggregationExpression` (3), `InvalidDelete`, `InvalidParameterUse`, `RelationshipUniquenessViolation`, `NoSingleRelationshipType`, `UnknownFunction` (1 each) | 15 |
| Full `EXISTS { }` subqueries (with `WITH`/`RETURN` or nested clauses) | 5 |
| `<-[]->` relationship patterns | 3 |
| Reading a deleted node's labels does not raise `EntityNotFound` | 1 |
| A result over the 1,000,000-row cap | 1 |
