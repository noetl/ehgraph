//! Memory budgets, resolved for the mode an instance runs in (register C11/E5,
//! noetl/ehgraph#6).
//!
//! An instance holds three kinds of memory:
//!
//! | Kind | Bounded by | On overflow |
//! |---|---|---|
//! | the **materialized graph** (always resident, every namespace) | `resident_graph_bytes` | a growing write is refused with `MEMORY_BUDGET_EXCEEDED`; nothing is logged |
//! | **sessions** (Arrow batches per open session) | `global_bytes`, `per_session_bytes` | ended sessions are evicted first, then the open fails with `MEMORY_BUDGET_EXCEEDED` |
//! | **idempotency keys** | `[store] idempotency_max_keys` (C23) | the oldest keys are forgotten |
//!
//! Plus transient planning memory per write, bounded by the batch.
//!
//! # Defaults
//!
//! **Standalone**: the pod is ehgraph's own, so the defaults are fixed (sessions
//! 256 MiB global / 64 MiB each, graph unlimited) and the pod's limit is sized for
//! them.
//!
//! **Embedded**: the memory is the host's. ehgraph reads the host's memory limit
//! (`[budgets] host_memory_bytes`, else the cgroup limit of the container it runs in)
//! and takes, for whatever is not set explicitly:
//!
//! - `resident_graph_bytes` = 10% of the host,
//! - `global_bytes` = 10% of the host, `per_session_bytes` = a quarter of that,
//!
//! each at least 16 MiB. It then **refuses to open** if the graph and session budgets
//! together exceed 50% of the host, explicit values included, so a mis-sized embedded
//! graph fails at start rather than OOM-killing the host later. With no detectable
//! limit (a laptop) the embedded defaults are the fixed ones and graph is capped at
//! 256 MiB, with a warning.

use ehgraph_core::EhError;
use serde::Deserialize;

const MIB: u64 = 1024 * 1024;
const FLOOR: u64 = 16 * MIB;
/// Anything at or above this is "no limit" (cgroup v1 reports ~2^63).
const NO_LIMIT: u64 = 1 << 60;

/// The `[budgets]` section. Every field is optional; see [`resolve`].
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    /// All sessions together.
    #[serde(default)]
    pub global_bytes: Option<u64>,
    /// One session.
    #[serde(default)]
    pub per_session_bytes: Option<u64>,
    /// The materialized graphs of every namespace together. `0` is unlimited.
    #[serde(default)]
    pub resident_graph_bytes: Option<u64>,
    /// Embedded: the host's memory limit. Overrides the detected cgroup limit.
    #[serde(default)]
    pub host_memory_bytes: Option<u64>,
}

/// Budgets after defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    pub global_bytes: u64,
    pub per_session_bytes: u64,
    /// `0` is unlimited.
    pub resident_graph_bytes: u64,
    /// The host limit the embedded defaults were derived from, if any.
    pub host_memory_bytes: Option<u64>,
}

/// Resolve `config` for an embedded (`true`) or standalone instance. `detected` is
/// the container's memory limit, from [`detect_host_memory`].
pub fn resolve(
    config: &BudgetConfig,
    embedded: bool,
    detected: Option<u64>,
) -> Result<Budgets, EhError> {
    let budgets = if !embedded {
        Budgets {
            global_bytes: config.global_bytes.unwrap_or(256 * MIB),
            per_session_bytes: config.per_session_bytes.unwrap_or(64 * MIB),
            resident_graph_bytes: config.resident_graph_bytes.unwrap_or(0),
            host_memory_bytes: None,
        }
    } else {
        let host = config
            .host_memory_bytes
            .or(detected)
            .filter(|&bytes| bytes > 0 && bytes < NO_LIMIT);
        match host {
            Some(host) => {
                let global = config.global_bytes.unwrap_or((host / 10).max(FLOOR));
                Budgets {
                    global_bytes: global,
                    per_session_bytes: config.per_session_bytes.unwrap_or((global / 4).max(FLOOR)),
                    resident_graph_bytes: config
                        .resident_graph_bytes
                        .unwrap_or((host / 10).max(FLOOR)),
                    host_memory_bytes: Some(host),
                }
            }
            None => {
                tracing::warn!(
                    "embedded ehgraph: no host memory limit set ([budgets] host_memory_bytes) or \
                     detected (cgroup); using fixed budgets"
                );
                Budgets {
                    global_bytes: config.global_bytes.unwrap_or(256 * MIB),
                    per_session_bytes: config.per_session_bytes.unwrap_or(64 * MIB),
                    resident_graph_bytes: config.resident_graph_bytes.unwrap_or(256 * MIB),
                    host_memory_bytes: None,
                }
            }
        }
    };
    if budgets.per_session_bytes > budgets.global_bytes {
        return Err(EhError::invalid(format!(
            "[budgets] per_session_bytes ({}) exceeds global_bytes ({})",
            budgets.per_session_bytes, budgets.global_bytes
        )));
    }
    if budgets.global_bytes == 0 {
        return Err(EhError::invalid(
            "[budgets] global_bytes must be at least 1",
        ));
    }
    if let Some(host) = budgets.host_memory_bytes {
        if budgets.resident_graph_bytes == 0 {
            return Err(EhError::invalid(
                "embedded ehgraph refuses an unlimited resident graph (resident_graph_bytes = 0) \
                 when the host has a memory limit; set a cap",
            ));
        }
        let total = budgets.global_bytes + budgets.resident_graph_bytes;
        if total > host / 2 {
            return Err(EhError::invalid(format!(
                "embedded ehgraph budgets would take {} MiB (sessions {} + graph {}) of the \
                 host's {} MiB limit; at most half is allowed. Lower [budgets] or raise the \
                 host's memory limit",
                total / MIB,
                budgets.global_bytes / MIB,
                budgets.resident_graph_bytes / MIB,
                host / MIB
            )));
        }
    }
    Ok(budgets)
}

/// The memory limit of the container this process runs in (cgroup v2, then v1), or
/// `None` when there is none or it cannot be read.
pub fn detect_host_memory() -> Option<u64> {
    for path in [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ] {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Some(limit) = parse_cgroup_limit(&text) {
                return Some(limit);
            }
        }
    }
    None
}

/// Parse a cgroup memory limit file. `max` and v1's near-2^63 sentinel are "none".
pub fn parse_cgroup_limit(text: &str) -> Option<u64> {
    let value = text.trim();
    if value == "max" {
        return None;
    }
    value.parse::<u64>().ok().filter(|&v| v > 0 && v < NO_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * MIB;

    #[test]
    fn standalone_keeps_the_fixed_defaults_and_an_unlimited_graph() {
        let b = resolve(&BudgetConfig::default(), false, Some(GIB)).unwrap();
        assert_eq!(b.global_bytes, 256 * MIB);
        assert_eq!(b.per_session_bytes, 64 * MIB);
        assert_eq!(b.resident_graph_bytes, 0);
    }

    #[test]
    fn embedded_derives_from_the_host_limit() {
        let b = resolve(&BudgetConfig::default(), true, Some(4 * GIB)).unwrap();
        assert_eq!(b.resident_graph_bytes, 4 * GIB / 10);
        assert_eq!(b.global_bytes, 4 * GIB / 10);
        assert_eq!(b.per_session_bytes, 4 * GIB / 40);
        assert_eq!(b.host_memory_bytes, Some(4 * GIB));
        // An explicit host limit wins over the detected one.
        let cfg = BudgetConfig {
            host_memory_bytes: Some(2 * GIB),
            ..Default::default()
        };
        assert_eq!(
            resolve(&cfg, true, Some(64 * GIB))
                .unwrap()
                .host_memory_bytes,
            Some(2 * GIB)
        );
    }

    #[test]
    fn embedded_refuses_budgets_over_half_the_host() {
        let cfg = BudgetConfig {
            resident_graph_bytes: Some(900 * MIB),
            ..Default::default()
        };
        let err = resolve(&cfg, true, Some(2 * GIB)).unwrap_err();
        assert!(err.message.contains("at most half"), "{}", err.message);
        let unlimited = BudgetConfig {
            resident_graph_bytes: Some(0),
            ..Default::default()
        };
        assert!(resolve(&unlimited, true, Some(2 * GIB)).is_err());
        // A tiny host cannot fit even the floors.
        assert!(resolve(&BudgetConfig::default(), true, Some(48 * MIB)).is_err());
    }

    #[test]
    fn embedded_without_a_limit_falls_back_and_caps_the_graph() {
        let b = resolve(&BudgetConfig::default(), true, None).unwrap();
        assert_eq!(b.resident_graph_bytes, 256 * MIB);
        assert_eq!(b.host_memory_bytes, None);
    }

    #[test]
    fn per_session_cannot_exceed_global() {
        let cfg = BudgetConfig {
            global_bytes: Some(10 * MIB),
            per_session_bytes: Some(20 * MIB),
            ..Default::default()
        };
        assert!(resolve(&cfg, false, None).is_err());
    }

    #[test]
    fn parses_cgroup_limits() {
        assert_eq!(parse_cgroup_limit("max\n"), None);
        assert_eq!(parse_cgroup_limit("2147483648\n"), Some(2 * GIB));
        assert_eq!(parse_cgroup_limit("9223372036854771712"), None);
        assert_eq!(parse_cgroup_limit("garbage"), None);
    }
}
