//! Performance harness for the targets in `docs/wiki/Performance.md` (register C17,
//! noetl/ehgraph#7).
//!
//! ```text
//! cargo bench -p ehgraph --bench perf -- --elements 100000
//! cargo bench -p ehgraph --bench perf -- --elements 20000 --json target/perf.json --check
//! ```
//!
//! It builds a `noetl-cluster`-shaped graph (services, pods, nodes; two edges per
//! node) through the same in-process dispatch both modes use, then times each tool
//! class. `--check` compares p95s with the targets scaled to the run's size and prints
//! `REGRESSION` lines; it exits non-zero only with `--strict`, so CI can run it
//! without gating at first.
//!
//! No external benchmark framework: the numbers are wall-clock percentiles over
//! repeated calls, which is what the targets are written in.

use std::path::Path;
use std::time::{Duration, Instant};

use ehgraph::{EhGraph, GraphConfig};
use serde_json::{json, Value};

struct Args {
    elements: usize,
    json: Option<String>,
    check: bool,
    strict: bool,
}

fn args() -> Args {
    let mut out = Args {
        elements: 100_000,
        json: None,
        check: false,
        strict: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--elements" => {
                out.elements = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(out.elements)
            }
            "--json" => out.json = it.next(),
            "--check" => out.check = true,
            "--strict" => out.strict = true,
            _ => {} // cargo passes --bench and filters; ignore them
        }
    }
    out
}

fn config(root: &Path) -> GraphConfig {
    GraphConfig::from_toml(&format!(
        r#"
instance_id = "ehgraph-bench"
domain = "bench"
data_dir = "{data}"
backup_prefix = "{backup}"

[auth]
mode = "disabled"

[budgets]
global_bytes = 8589934592
per_session_bytes = 4294967296
"#,
        data = root.join("data").display(),
        backup = root.join("backup").display(),
    ))
    .unwrap()
}

fn node(i: usize) -> Value {
    let kind = ["Service", "Pod", "Node"][i % 3];
    json!({
        "id": format!("{}:{i:07}", kind.to_lowercase()),
        "type": kind,
        "props": {
            "name": format!("{kind}-{i}"),
            "namespace": "noetl",
            "image": format!("europe-docker.pkg.dev/noetl/server-rust:v3.{}", i % 200),
            "replicas": (i % 5) as i64,
            "ready": i % 7 != 0,
        }
    })
}

fn node_id(i: usize) -> String {
    let kind = ["service", "pod", "node"][i % 3];
    format!("{kind}:{i:07}")
}

fn edges_for(i: usize, nodes: usize) -> Vec<Value> {
    // Two edges per node, to a near and a far neighbour: a sparse, connected graph.
    [(i + 1) % nodes, (i * 7 + 3) % nodes]
        .iter()
        .enumerate()
        .map(|(k, &j)| {
            let edge_type = ["DISPATCHES_TO", "RUNS_ON"][k];
            json!({
                "src": node_id(i),
                "dst": node_id(j),
                "type": edge_type,
            })
        })
        .collect()
}

#[derive(Default)]
struct Samples(Vec<Duration>);

impl Samples {
    fn time<T>(&mut self, f: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let out = f();
        self.0.push(start.elapsed());
        out
    }
    fn pct(&self, p: f64) -> f64 {
        let mut v: Vec<f64> = self.0.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        if v.is_empty() {
            return 0.0;
        }
        v[((v.len() - 1) as f64 * p).round() as usize]
    }
}

struct Row {
    name: &'static str,
    unit: &'static str,
    p50: f64,
    p95: f64,
    /// The target for this run's size, in `unit`.
    target: f64,
    note: String,
}

fn ok(envelope: &ehgraph::Envelope, what: &str) {
    assert!(envelope.ok, "{what}: {envelope:?}");
}

fn main() {
    let args = args();
    let nodes = (args.elements / 3).max(10);
    let root = tempfile::tempdir().unwrap();
    let graph = EhGraph::open_standalone(config(root.path())).unwrap();
    let call = |tool: &str, a: Value| graph.call("", tool, &a);
    let mut rows = Vec::new();
    // Targets are written for 10^6 elements; reads are size-independent, writes and
    // open scale linearly.
    let scale = args.elements as f64 / 1_000_000.0;

    // Load: batches of 1,000 nodes and their 2,000 edges.
    let mut load = Samples::default();
    let load_start = Instant::now();
    for chunk in (0..nodes).collect::<Vec<_>>().chunks(1_000) {
        let batch = json!({
            "namespace": "cluster",
            "source": "bench",
            "execution_id": "load",
            "nodes": chunk.iter().map(|&i| node(i)).collect::<Vec<_>>(),
        });
        ok(&load.time(|| call("graph.upsert", batch)), "load nodes");
    }
    for chunk in (0..nodes).collect::<Vec<_>>().chunks(1_000) {
        let batch = json!({
            "namespace": "cluster",
            "source": "bench",
            "execution_id": "load",
            "edges": chunk.iter().flat_map(|&i| edges_for(i, nodes)).collect::<Vec<_>>(),
        });
        ok(&load.time(|| call("graph.upsert", batch)), "load edges");
    }
    let loaded = load_start.elapsed().as_secs_f64();
    let elements = nodes * 3;
    rows.push(Row {
        name: "load throughput",
        unit: "elements/s",
        p50: elements as f64 / loaded,
        p95: elements as f64 / loaded,
        target: 20_000.0,
        note: format!(
            "{elements} elements in {loaded:.1} s; batch p95 {:.1} ms",
            load.pct(0.95)
        ),
    });

    let mut rng = 0x9e3779b97f4a7c15u64;
    let mut pick = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng % nodes as u64) as usize
    };

    let mut point = Samples::default();
    for _ in 0..200 {
        let a = json!({"namespace": "cluster", "id": node_id(pick())});
        ok(&point.time(|| call("graph.node", a)), "graph.node");
    }
    rows.push(Row {
        name: "graph.node (point read)",
        unit: "ms",
        p50: point.pct(0.5),
        p95: point.pct(0.95),
        target: 5.0,
        note: String::new(),
    });

    let mut neighbors = Samples::default();
    for _ in 0..200 {
        let a = json!({"namespace": "cluster", "node_id": node_id(pick()), "direction": "both", "limit": 100});
        ok(
            &neighbors.time(|| call("graph.neighbors", a)),
            "graph.neighbors",
        );
    }
    rows.push(Row {
        name: "graph.neighbors",
        unit: "ms",
        p50: neighbors.pct(0.5),
        p95: neighbors.pct(0.95),
        target: 10.0,
        note: String::new(),
    });

    let mut traversal = Samples::default();
    for _ in 0..50 {
        let a =
            json!({"namespace": "cluster", "seeds": [node_id(pick())], "k": 2, "max_nodes": 500});
        ok(
            &traversal.time(|| call("graph.subgraph", a)),
            "graph.subgraph",
        );
    }
    rows.push(Row {
        name: "graph.subgraph (k=2)",
        unit: "ms",
        p50: traversal.pct(0.5),
        p95: traversal.pct(0.95),
        target: 50.0,
        note: String::new(),
    });

    let mut small = Samples::default();
    for round in 0..100 {
        let ids: Vec<usize> = (0..10).map(|_| pick()).collect();
        let batch = json!({
            "namespace": "cluster",
            "source": "bench",
            "execution_id": format!("tick-{round}"),
            "nodes": ids.iter().map(|&i| {
                let mut n = node(i);
                n["props"]["replicas"] = json!(round as i64);
                n
            }).collect::<Vec<_>>(),
        });
        ok(&small.time(|| call("graph.upsert", batch)), "small upsert");
    }
    rows.push(Row {
        name: "graph.upsert (10 nodes, warm)",
        unit: "ms",
        p50: small.pct(0.5),
        p95: small.pct(0.95),
        target: 20.0,
        note: "fsync per commit".into(),
    });

    // The cached head view is rebuilt after every write, so a read right after a write
    // pays one O(namespace) build. Point reads (graph.node/edge) never do.
    let mut after_write = Samples::default();
    let mut point_after_write = Samples::default();
    for round in 0..10 {
        let batch = json!({
            "namespace": "cluster", "source": "bench", "execution_id": format!("rw-{round}"),
            "nodes": [{"id": node_id(pick()), "type": "Pod", "props": {"tick": round as i64}}],
        });
        ok(&call("graph.upsert", batch), "rw upsert");
        let a = json!({"namespace": "cluster", "id": node_id(pick())});
        ok(&point_after_write.time(|| call("graph.node", a)), "rw node");
        let a = json!({"namespace": "cluster", "node_id": node_id(pick()), "direction": "both", "limit": 100});
        ok(
            &after_write.time(|| call("graph.neighbors", a)),
            "rw neighbors",
        );
    }
    rows.push(Row {
        name: "graph.node right after a write",
        unit: "ms",
        p50: point_after_write.pct(0.5),
        p95: point_after_write.pct(0.95),
        target: 5.0,
        note: String::new(),
    });
    rows.push(Row {
        name: "graph.neighbors right after a write",
        unit: "ms",
        p50: after_write.pct(0.5),
        p95: after_write.pct(0.95),
        target: 2_000.0 * scale,
        note: "rebuilds the head view; scales with the graph".into(),
    });

    // Embedded: the same call through the host's blocking pool (`call_async`), which is
    // how noetl-server reaches it. The difference is the cost the host's async
    // workers see per call.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut embedded = Samples::default();
    for _ in 0..200 {
        let a = json!({"namespace": "cluster", "id": node_id(pick())});
        let g = graph.clone();
        let start = Instant::now();
        let envelope = runtime
            .block_on(async move { g.call_async(String::new(), "graph.node".into(), a).await });
        embedded.0.push(start.elapsed());
        ok(&envelope, "call_async graph.node");
    }
    rows.push(Row {
        name: "graph.node via call_async (embedded)",
        unit: "ms",
        p50: embedded.pct(0.5),
        p95: embedded.pct(0.95),
        target: 5.0,
        note: "blocking-pool hop included".into(),
    });
    drop(runtime);

    let mut session = Samples::default();
    for i in 0..3 {
        let a = json!({"namespace": "cluster", "owner": format!("bench-{i}"), "scope": {"kind": "full"}});
        let opened = session.time(|| call("graph.session.open", a));
        ok(&opened, "session.open");
        let id = opened.data.as_ref().unwrap()["graph_session_id"]
            .as_str()
            .unwrap()
            .to_string();
        ok(
            &call("graph.session.close", json!({"graph_session_id": id})),
            "session.close",
        );
    }
    rows.push(Row {
        name: "graph.session.open (full)",
        unit: "ms",
        p50: session.pct(0.5),
        p95: session.pct(0.95),
        target: 5_000.0 * scale,
        note: "scales with the graph".into(),
    });

    let mut session_reads = Samples::default();
    let opened = call(
        "graph.session.open",
        json!({"namespace": "cluster", "owner": "bench-read", "scope": {"kind": "full"}}),
    );
    let sid = opened.data.as_ref().unwrap()["graph_session_id"]
        .as_str()
        .unwrap()
        .to_string();
    for _ in 0..200 {
        let a = json!({"namespace": "cluster", "graph_session_id": sid, "id": node_id(pick())});
        ok(
            &session_reads.time(|| call("graph.node", a)),
            "session graph.node",
        );
    }
    rows.push(Row {
        name: "graph.node in a session",
        unit: "ms",
        p50: session_reads.pct(0.5),
        p95: session_reads.pct(0.95),
        target: 1.0,
        note: String::new(),
    });

    drop(graph);
    let mut reopen = Samples::default();
    let graph = reopen.time(|| EhGraph::open_standalone(config(root.path())).unwrap());
    rows.push(Row {
        name: "open (replay)",
        unit: "ms",
        p50: reopen.pct(0.5),
        p95: reopen.pct(0.95),
        target: 30_000.0 * scale,
        note: "scales with the log".into(),
    });
    let resident: u64 = graph
        .metrics_text()
        .lines()
        .filter(|l| l.starts_with("ehgraph_resident_graph_bytes{"))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
        .sum();
    rows.push(Row {
        name: "resident graph",
        unit: "B/element",
        p50: resident as f64 / elements as f64,
        p95: resident as f64 / elements as f64,
        target: 2_000.0,
        note: format!("{:.0} MiB estimated", resident as f64 / 1048576.0),
    });

    println!(
        "ehgraph perf, {elements} elements ({nodes} nodes, {} edges)",
        nodes * 2
    );
    println!(
        "{:<34} {:>12} {:>12} {:>12}  unit",
        "case", "p50", "p95", "target"
    );
    let mut regressions = 0;
    let mut out = Vec::new();
    for row in &rows {
        // Throughput is "higher is better"; everything else is a ceiling.
        let regressed = if row.unit == "elements/s" {
            row.p95 < row.target
        } else {
            row.p95 > row.target
        };
        if regressed {
            regressions += 1;
        }
        println!(
            "{:<34} {:>12.2} {:>12.2} {:>12.2}  {} {}{}",
            row.name,
            row.p50,
            row.p95,
            row.target,
            row.unit,
            row.note,
            if args.check && regressed {
                "  REGRESSION"
            } else {
                ""
            }
        );
        out.push(json!({"case": row.name, "unit": row.unit, "p50": row.p50, "p95": row.p95, "target": row.target, "regressed": regressed}));
    }
    if let Some(path) = &args.json {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&json!({"elements": elements, "results": out})).unwrap(),
        )
        .unwrap();
    }
    if args.check && regressions > 0 {
        println!("{regressions} case(s) over target");
        if args.strict {
            std::process::exit(1);
        }
    }
}
