//! How a host (noetl-server) embeds one ehgraph instance.
//!
//! ```text
//! cargo run -p ehgraph --example embed_in_host -- /tmp/ehgraph-embed
//! ```
//!
//! The host owns the runtime, the listener, tracing and signals. ehgraph owns its
//! data directory (locked for the handle's lifetime), the change log, sessions,
//! and grants.

use std::time::Duration;

use ehgraph::{EhGraph, GraphConfig};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/ehgraph-embed".into());

    // In noetl-server this is a sub-table of its own config.
    let config = GraphConfig::from_toml(&format!(
        r#"
instance_id = "ehgraph-noetl-cluster"
domain = "noetl-cluster"
data_dir = "{root}/data"
backup_prefix = "{root}/backup"

# E2: the host declares it is the only writer. Without this the open refuses.
[embedded]
host_replicas = 1

[[auth.grants]]
token = "noetl-server-internal"
principal = "noetl-server"
namespace = "*"
read = true
write = true
"#
    ))?;

    // 1. Open off the async workers: it locks the directory and replays the log.
    let graph = EhGraph::open_embedded_async(config).await?;
    // 2. Session sweep and scheduled backup on the HOST's runtime.
    graph.start_maintenance()?;

    // 3a. In-process calls: no network hop, same grants as HTTP.
    let client = graph.client("noetl-server-internal")?;
    let info = client.call_async("graph.instance.info", json!({})).await;
    println!("{}", serde_json::to_string_pretty(&info)?);

    // 3b. The same instance over MCP/HTTP, mounted on the host's own router, so
    //     `kind: mcp` playbooks reach it at <host>/ehgraph/noetl-cluster/mcp.
    let _host_router: axum::Router =
        axum::Router::new().nest("/ehgraph/noetl-cluster", graph.router());

    // 4. On host shutdown: stop maintenance, drain, release the lock.
    let report = graph.shutdown(Duration::from_secs(10)).await;
    println!("lock released: {}", report.lock_released);
    Ok(())
}
