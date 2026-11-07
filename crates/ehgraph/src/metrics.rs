use ehgraph_api::Engine;
use ehgraph_core::now_ms;

/// Prometheus text for one instance. `mode` is added only as the separate
/// `ehgraph_info` series, so every existing series keeps its exact label set.
pub fn render(engine: &Engine, mode: &str) -> String {
    let instance = &engine.identity.instance_id;
    let domain = &engine.identity.domain;
    let mut out = String::new();
    out.push_str("# TYPE ehgraph_info gauge\n");
    out.push_str(&format!(
        "ehgraph_info{{instance_id=\"{instance}\",domain=\"{domain}\",mode=\"{mode}\"}} 1\n"
    ));
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
    out.push_str("# TYPE ehgraph_budget_bytes gauge\n");
    for (kind, value) in [
        ("resident_graph", engine.resident_graph_bytes),
        ("sessions_global", engine.global_bytes),
        ("session", engine.per_session_bytes),
    ] {
        out.push_str(&format!(
            "ehgraph_budget_bytes{{instance_id=\"{instance}\",domain=\"{domain}\",kind=\"{kind}\"}} {value}\n"
        ));
    }
    // Cached: a scrape never reads the backup target (it may be a bucket).
    let checkpoint = engine.backup.cached_checkpoint();
    out.push_str("# TYPE ehgraph_backup_failures_total counter\n");
    out.push_str(&format!(
        "ehgraph_backup_failures_total{{instance_id=\"{instance}\",domain=\"{domain}\"}} {}\n",
        engine.backup.failures()
    ));
    out.push_str("# TYPE ehgraph_backup_last_shipped_timestamp_seconds gauge\n");
    out.push_str(&format!(
        "ehgraph_backup_last_shipped_timestamp_seconds{{instance_id=\"{instance}\",domain=\"{domain}\"}} {}\n",
        checkpoint.as_ref().map(|c| c.shipped_at_ms / 1000).unwrap_or(0)
    ));
    let now = now_ms();
    if let Ok(summaries) = engine.store.summaries() {
        out.push_str("# TYPE ehgraph_graph_version gauge\n");
        out.push_str("# TYPE ehgraph_backup_lag_versions gauge\n");
        out.push_str("# TYPE ehgraph_backup_lag_seconds gauge\n");
        out.push_str("# TYPE ehgraph_resident_graph_bytes gauge\n");
        out.push_str("# TYPE ehgraph_idempotency_keys gauge\n");
        out.push_str("# TYPE ehgraph_idempotency_key_bytes gauge\n");
        out.push_str("# TYPE ehgraph_idempotency_keys_evicted counter\n");
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
            out.push_str(&format!(
                "ehgraph_resident_graph_bytes{{instance_id=\"{instance}\",domain=\"{domain}\",namespace=\"{ns}\"}} {}\n",
                summary.graph_bytes
            ));
            out.push_str(&format!(
                "ehgraph_idempotency_keys{{instance_id=\"{instance}\",domain=\"{domain}\",namespace=\"{ns}\"}} {}\n",
                summary.idempotency_keys
            ));
            out.push_str(&format!(
                "ehgraph_idempotency_key_bytes{{instance_id=\"{instance}\",domain=\"{domain}\",namespace=\"{ns}\"}} {}\n",
                summary.idempotency_key_bytes
            ));
            out.push_str(&format!(
                "ehgraph_idempotency_keys_evicted{{instance_id=\"{instance}\",domain=\"{domain}\",namespace=\"{ns}\"}} {}\n",
                summary.idempotency_keys_evicted
            ));
        }
    }
    out
}
