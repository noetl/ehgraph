use ehgraph_api::Engine;
use ehgraph_core::now_ms;

pub fn render(engine: &Engine) -> String {
    let instance = &engine.identity.instance_id;
    let domain = &engine.identity.domain;
    let mut out = String::new();
    let counts = engine.sessions.counts().unwrap_or_default();
    out.push_str("# TYPE ehgraph_sessions gauge\n");
    for state in [
        "loading",
        "active",
        "refreshing",
        "closed",
        "expired",
        "evicted",
        "failed",
    ] {
        let value = counts.get(state).copied().unwrap_or(0);
        out.push_str(&format!(
            "ehgraph_sessions{{instance_id=\"{instance}\",domain=\"{domain}\",state=\"{state}\"}} {value}\n"
        ));
    }
    let resident = engine.sessions.resident_bytes().unwrap_or(0);
    out.push_str("# TYPE ehgraph_resident_bytes gauge\n");
    out.push_str(&format!(
        "ehgraph_resident_bytes{{instance_id=\"{instance}\",domain=\"{domain}\"}} {resident}\n"
    ));
    let load_seconds = engine.sessions.last_load_ms().unwrap_or(0) as f64 / 1000.0;
    out.push_str("# TYPE ehgraph_load_seconds gauge\n");
    out.push_str(&format!(
        "ehgraph_load_seconds{{instance_id=\"{instance}\",domain=\"{domain}\"}} {load_seconds}\n"
    ));
    out.push_str("# TYPE ehgraph_evictions_total counter\n");
    let evictions = engine.sessions.evictions().unwrap_or_default();
    if evictions.is_empty() {
        out.push_str(&format!(
            "ehgraph_evictions_total{{instance_id=\"{instance}\",domain=\"{domain}\",reason=\"budget\"}} 0\n"
        ));
    } else {
        for (reason, value) in evictions {
            out.push_str(&format!(
                "ehgraph_evictions_total{{instance_id=\"{instance}\",domain=\"{domain}\",reason=\"{reason}\"}} {value}\n"
            ));
        }
    }
    let checkpoint = ehgraph_backup::read_checkpoint(&engine.backup_prefix)
        .ok()
        .flatten();
    let now = now_ms();
    if let Ok(summaries) = engine.store.summaries() {
        out.push_str("# TYPE ehgraph_graph_version gauge\n");
        out.push_str("# TYPE ehgraph_backup_lag_versions gauge\n");
        out.push_str("# TYPE ehgraph_backup_lag_seconds gauge\n");
        for summary in summaries {
            let ns = summary.namespace;
            out.push_str(&format!(
                "ehgraph_graph_version{{instance_id=\"{instance}\",domain=\"{domain}\",namespace=\"{ns}\"}} {}\n",
                summary.head_version
            ));
            let saved = checkpoint
                .as_ref()
                .and_then(|item| item.namespaces.get(&ns));
            let (lag_versions, lag_seconds) = ehgraph_backup::lag(summary.head_version, saved, now);
            out.push_str(&format!(
                "ehgraph_backup_lag_versions{{instance_id=\"{instance}\",domain=\"{domain}\",namespace=\"{ns}\"}} {lag_versions}\n"
            ));
            out.push_str(&format!(
                "ehgraph_backup_lag_seconds{{instance_id=\"{instance}\",domain=\"{domain}\",namespace=\"{ns}\"}} {lag_seconds}\n"
            ));
        }
    }
    out
}
