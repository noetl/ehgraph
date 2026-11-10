//! Semantics of the openCypher subset over an in-memory view (no server).

use std::collections::BTreeMap;

use ehgraph_core::{Edge, Node, PropValue};
use ehgraph_cypher::{execute, parse, ExecOptions, Val};
use ehgraph_memory::SessionGraph;
use ehgraph_store::MaterializedGraph;
use serde_json::{json, Value};

fn node(id: &str, t: &str, props: &[(&str, PropValue)]) -> Node {
    Node {
        id: id.into(),
        type_name: t.into(),
        props: props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        first_seen: 1,
        last_seen: 1,
        active: true,
        origin: None,
        source: "t".into(),
        execution_id: "t".into(),
        version_created: 1,
        version_updated: 1,
    }
}

fn edge(src: &str, t: &str, dst: &str) -> Edge {
    Edge {
        id: ehgraph_core::default_edge_id(src, t, dst, None),
        src: src.into(),
        dst: dst.into(),
        type_name: t.into(),
        props: BTreeMap::new(),
        first_seen: 1,
        last_seen: 1,
        active: true,
        origin: None,
        source: "t".into(),
        execution_id: "t".into(),
        version_created: 1,
        version_updated: 1,
    }
}

/// a -> b -> c -> a (a cycle), plus a self-loop on c.
fn graph() -> SessionGraph {
    let nodes = vec![
        node("a", "N", &[("v", PropValue::Int(1))]),
        node("b", "N", &[("v", PropValue::Int(2))]),
        node("c", "M", &[("v", PropValue::Float(2.5))]),
    ];
    let edges = vec![
        edge("a", "R", "b"),
        edge("b", "R", "c"),
        edge("c", "R", "a"),
        edge("c", "SELF", "c"),
    ];
    SessionGraph::from_materialized(&MaterializedGraph::from_elements(1, nodes, edges)).unwrap()
}

fn run(q: &str) -> Vec<Value> {
    let out = execute(
        &graph(),
        &parse(q).unwrap(),
        &BTreeMap::new(),
        &ExecOptions::default(),
    )
    .unwrap_or_else(|e| panic!("{q}: {e}"));
    out.rows_json()
}

fn err(q: &str) -> String {
    match parse(q) {
        Err(e) => e.to_string(),
        Ok(query) => execute(&graph(), &query, &BTreeMap::new(), &ExecOptions::default())
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| panic!("{q} should fail")),
    }
}

#[test]
fn parses_comments_backticks_case_insensitive_keywords() {
    let got = run("match (`n`:N) // comment\n /* block */ where n.v = 1 return n.v as `the value`");
    assert_eq!(got, vec![json!([1])]);
}

#[test]
fn variable_length_bounds_and_trail_semantics() {
    // Exactly the cycle from a back to a: 3 hops, no edge reused.
    assert_eq!(
        run("MATCH p = (a {id:'a'})-[:R*]->(a) RETURN length(p)"),
        vec![json!([3])]
    );
    // *0.. includes the start itself.
    assert_eq!(
        run("MATCH (a {id:'a'})-[:R*0..1]->(x) RETURN x.id ORDER BY x.id"),
        vec![json!(["a"]), json!(["b"])]
    );
    // *..2 means 1..2.
    assert_eq!(
        run("MATCH (a {id:'a'})-[:R*..2]->(x) RETURN x.id ORDER BY x.id"),
        vec![json!(["b"]), json!(["c"])]
    );
    // A relationship is never bound twice in one MATCH.
    assert_eq!(
        run("MATCH (x)-[r1:R]->(y)-[r2:R]->(z) WHERE x.id = 'a' RETURN z.id"),
        vec![json!(["c"])]
    );
    // A self-loop is matched once undirected.
    assert_eq!(
        run("MATCH (c {id:'c'})-[r:SELF]-(x) RETURN count(r)"),
        vec![json!([1])]
    );
    // Incoming direction.
    assert_eq!(
        run("MATCH (b {id:'b'})<-[:R]-(x) RETURN x.id"),
        vec![json!(["a"])]
    );
    // Type alternation.
    assert_eq!(
        run("MATCH (c {id:'c'})-[:R|SELF]->(x) RETURN count(*)"),
        vec![json!([2])]
    );
}

#[test]
fn three_valued_logic_and_null_handling() {
    assert_eq!(
        run("RETURN null = null AS a, null OR true AS b, null AND false AS c, NOT null AS d"),
        vec![json!([null, true, false, null])]
    );
    assert_eq!(
        run("RETURN 1 IN [null, 1] AS a, 2 IN [null, 1] AS b, [] IS NULL AS c"),
        vec![json!([true, null, false])]
    );
    assert_eq!(
        run("MATCH (n) WHERE n.missing = 1 RETURN n"),
        Vec::<Value>::new()
    );
    assert_eq!(
        run("RETURN 1 = 1.0 AS a, 'a' < 'b' AS b, 1 < 'a' AS c"),
        vec![json!([true, true, null])]
    );
}

#[test]
fn aggregation_grouping_and_empty_input() {
    assert_eq!(
        run("MATCH (n:Nope) RETURN count(n), collect(n), sum(n.v), avg(n.v), max(n.v)"),
        vec![json!([0, [], 0, null, null])]
    );
    assert_eq!(
        run("MATCH (n:Nope) RETURN n.v, count(*)"),
        Vec::<Value>::new()
    );
    assert_eq!(
        run("MATCH (n) RETURN labels(n)[0] AS l, sum(n.v) AS s, min(n.v) AS lo ORDER BY l"),
        vec![json!(["M", 2.5, 2.5]), json!(["N", 3, 1])]
    );
    assert_eq!(
        run("MATCH (n) RETURN count(*) * 2 + 1 AS x"),
        vec![json!([7])]
    );
    assert_eq!(
        run("MATCH (n) WITH labels(n)[0] AS l, count(*) AS c ORDER BY c DESC LIMIT 1 RETURN l, c"),
        vec![json!(["N", 2])]
    );
}

#[test]
fn expressions_functions_and_comprehensions() {
    assert_eq!(
        run("RETURN [x IN range(1, 6) WHERE x % 2 = 0 | x * 10] AS l"),
        vec![json!([[20, 40, 60]])]
    );
    assert_eq!(run("RETURN all(x IN [1,2] WHERE x > 0) AS a, any(x IN [1,2] WHERE x > 1) AS b, none(x IN [1] WHERE x > 1) AS c, single(x IN [1,2] WHERE x > 1) AS d"), vec![json!([true, true, true, true])]);
    assert_eq!(run("RETURN toUpper('ab') + toString(1) AS s, size('héllo') AS n, split('a,b', ',') AS p, [1,2,3][1..] AS t, [1,2,3][-1] AS l"), vec![json!(["AB1", 5, ["a", "b"], [2, 3], 3])]);
    assert_eq!(run("RETURN coalesce(null, 'x') AS c, toInteger('42') AS i, 7 / 2 AS d, 7 / 2.0 AS f, 2 ^ 3 AS p"), vec![json!(["x", 42, 3, 3.5, 8.0])]);
    assert_eq!(
        run("WITH {a: {b: 1}} AS m RETURN m.a.b AS x, keys(m) AS k"),
        vec![json!([1, ["a"]])]
    );
    assert_eq!(
        run("MATCH (n:N) WHERE n:N RETURN count(n)"),
        vec![json!([2])]
    );
}

#[test]
fn writes_produce_net_changes_only() {
    let g = graph();
    let opts = ExecOptions::default();
    let params = BTreeMap::from([(
        "rows".to_string(),
        Val::from_json(&json!([{"id": "a", "v": 1}, {"id": "z", "v": 9}])),
    )]);
    let q = parse("UNWIND $rows AS r MERGE (n:N {id: r.id}) SET n.v = r.v").unwrap();
    let out = execute(&g, &q, &params, &opts).unwrap();
    // `a` already has v = 1: no change. Only `z` is written.
    assert_eq!(out.changes.nodes.len(), 1);
    assert_eq!(out.changes.nodes[0].id, "z");
    assert!(out.changes.deactivate.is_empty());

    // Create-then-delete inside one query nets to nothing.
    let q = parse("CREATE (t:T {id: 'tmp'}) WITH t DELETE t").unwrap();
    assert!(execute(&g, &q, &BTreeMap::new(), &opts)
        .unwrap()
        .changes
        .is_empty());

    // DETACH DELETE: node + every incident edge, as deactivations.
    let q = parse("MATCH (c {id: 'c'}) DETACH DELETE c").unwrap();
    let out = execute(&g, &q, &BTreeMap::new(), &opts).unwrap();
    assert_eq!(out.changes.deactivate.len(), 4);
}

#[test]
fn errors_are_typed() {
    assert!(err("MATCH (n) RETURN").starts_with("cypher syntax"));
    assert!(err("MATCH (n) RETURN n UNION RETURN 1").starts_with("cypher unsupported"));
    assert!(err("CALL db.labels()").starts_with("cypher unsupported"));
    assert!(err("RETURN nosuch(1)").starts_with("cypher unsupported"));
    assert!(err("RETURN x").contains("not defined"));
    assert!(err("RETURN 1 + 'a' - 1").starts_with("cypher type"));
    assert!(err("MATCH (n) RETURN n.v AS a, n.v AS a").contains("multiple result columns"));
    assert!(err("MATCH (n)").contains("must end with RETURN"));
}
