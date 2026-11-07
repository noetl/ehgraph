use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ehgraph_core::{
    canonical_hash, now_ms, validate_namespace, ChangeBatch, EhError, ErrorCode, FrameBody,
    IdempotencyNote, SchemaConfig, Scope, StoredOp, WriteRequest,
};
use serde::{Deserialize, Serialize};

use crate::arrow_batch::{
    batches_to_ipc, edges_to_batch, ipc_to_edges, ipc_to_nodes, nodes_to_batch,
};
use crate::format::{current_format, decode_marked, legacy_format};
use crate::frame::{self, decode_part};
use crate::lock::{DirLock, IdentityClaim};
use crate::state::{
    absorb_frame, plan_deactivate, plan_upsert, stamp_ops, KeyLimits, KeyWindow, MaterializedGraph,
};

#[derive(Clone, Debug)]
pub struct StoreConfig {
    pub instance_id: String,
    pub domain: String,
    pub data_dir: PathBuf,
    pub seal_max_bytes: u64,
    pub seal_max_records: u64,
    /// Parts also seal when they are at least this old. `Duration::ZERO` seals after every commit.
    pub seal_max_age: Duration,
    pub snapshot_keep: usize,
    /// Also claim `instance_id` and `domain` process-wide ([`IdentityClaim`]), so a
    /// second primary for the same instance or domain in this process is refused
    /// even on another directory. Set for embedded instances (E2); a standalone
    /// process serves one instance and leaves it off.
    pub claim_identity: bool,
    /// How long and how many idempotency keys each namespace remembers (C23).
    pub idempotency: KeyLimits,
    /// Cap on the estimated resident bytes of all materialized graphs together (C11).
    /// A write that would grow them past it is refused with `MEMORY_BUDGET_EXCEEDED`
    /// before anything is logged. `0` is unlimited. Open never refuses: the log is the
    /// authority, so an instance already over the cap opens and only refuses growth.
    pub resident_graph_bytes: u64,
}

impl StoreConfig {
    pub fn for_tests(data_dir: PathBuf) -> Self {
        Self {
            instance_id: "ehgraph-test".into(),
            domain: "test".into(),
            data_dir,
            seal_max_bytes: 8 * 1024 * 1024,
            seal_max_records: 1024,
            seal_max_age: Duration::from_secs(30),
            snapshot_keep: 16,
            claim_identity: false,
            idempotency: KeyLimits::default(),
            resident_graph_bytes: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WriteOutcome {
    pub graph_version: u64,
    pub changed: bool,
    pub applied: u64,
}

#[derive(Debug, Clone)]
pub struct DeactivateRequest {
    pub namespace: String,
    pub ids: Vec<String>,
    pub origin: Option<String>,
    pub source: Option<String>,
    pub not_seen_since_ms: Option<i64>,
    pub idempotency_key: Option<String>,
    pub execution_id: String,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct NsSummary {
    pub namespace: String,
    pub head_version: u64,
    pub sealed_version: u64,
    pub last_write_ms: i64,
    pub nodes: u64,
    pub edges: u64,
    /// Idempotency keys remembered now, their estimated bytes, and how many were
    /// forgotten since open (C23).
    pub idempotency_keys: u64,
    pub idempotency_key_bytes: u64,
    pub idempotency_keys_evicted: u64,
    /// Estimated resident bytes of the materialized graph (C11).
    pub graph_bytes: u64,
    /// Change log on disk: sealed parts listed in the manifest, and their bytes plus
    /// the active part's (C02).
    pub log_parts: u64,
    pub log_bytes: u64,
    /// Compaction floor; `0` when never compacted.
    pub floor_version: u64,
    /// How long the last open took to rebuild this namespace.
    pub replay_ms: u64,
}

/// What one [`Store::compact`] call did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompactionReport {
    pub namespace: String,
    /// Sealed parts removed from the log.
    pub dropped_parts: u64,
    pub dropped_bytes: u64,
    /// The floor after this call (unchanged when nothing was dropped).
    pub floor_version: u64,
    /// The full snapshot the floor rests on, if one was created or chosen.
    pub floor_snapshot: Option<String>,
    /// Why nothing was dropped, when nothing was.
    pub skipped: Option<String>,
}

/// Where a test makes [`Store::testing_compact_crash`] stop, simulating a crash.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactCrash {
    /// After the floor snapshot is written, before the new manifest.
    BeforeManifest,
    /// After the new manifest is durable, before the old parts are deleted.
    AfterManifest,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotInfo {
    pub snapshot_id: String,
    pub namespace: String,
    pub graph_version: u64,
    pub nodes: u64,
    pub edges: u64,
    pub bytes: u64,
    pub created_at_ms: i64,
    pub scope: Scope,
    pub full: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct VersionInfo {
    pub graph_version: u64,
    pub nodes: u64,
    pub edges: u64,
    pub bytes: u64,
    pub snapshot: Option<SnapshotInfo>,
}

#[derive(Debug, Clone)]
pub struct BackupPlan {
    pub instance_id: String,
    pub domain: String,
    pub namespaces: Vec<BackupNamespace>,
    pub instance_file: PathBuf,
    pub schema_files: Vec<(PathBuf, PathBuf)>,
}

#[derive(Debug, Clone)]
pub struct BackupNamespace {
    pub namespace: String,
    pub head_version: u64,
    pub sealed_version: u64,
    pub last_write_ms: i64,
    /// `(absolute source, path relative to data_dir)` for immutable files.
    pub files: Vec<(PathBuf, PathBuf)>,
    pub head_relative: PathBuf,
    /// Head pointer shipped last. `graph_version` is the sealed version.
    pub head_json: String,
}

/// One instance's durable directory. The process holds `data_dir/LOCK`.
pub struct Store {
    cfg: StoreConfig,
    // Field order is drop order: the directory lock goes before the identity claim,
    // so a reopen that wins the claim never finds the directory still locked.
    _lock: DirLock,
    _identity: Option<IdentityClaim>,
    inner: Mutex<Inner>,
}

struct Inner {
    namespaces: BTreeMap<String, Namespace>,
}

struct Namespace {
    head: u64,
    sealed_version: u64,
    manifest: Manifest,
    graph: MaterializedGraph,
    /// Estimated resident bytes of `graph` (`approx_bytes` of every element).
    graph_bytes: u64,
    keys: KeyWindow,
    active_records: u64,
    active_bytes: u64,
    active_opened_ms: i64,
    next_seq: u64,
    last_write_ms: i64,
    /// How long the last open took to rebuild this namespace (C02 metric).
    replay_ms: u64,
    /// Per sealed part, by seq: its highest commit `ts_ms`, and whether it records any
    /// idempotency key. Parts are immutable, so this is filled lazily by compaction
    /// and never invalidated.
    part_max_ts: HashMap<u64, (i64, bool)>,
}

// ⚠ Every struct below is read back from disk. `deny_unknown_fields` plus the
// top-level `format` marker make an older build refuse a file a newer one wrote
// (E9, see `format.rs`). Any shape change here bumps `STORE_FORMAT`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    #[serde(default = "legacy_format")]
    format: u32,
    version: u64,
    namespace: String,
    parts: Vec<PartMeta>,
    snapshots: Vec<SnapshotMeta>,
    /// Compaction floor (C02, format 2): every version at or below this was folded
    /// into `floor_snapshot` and its sealed parts were removed. `0` = never compacted.
    #[serde(default, skip_serializing_if = "is_zero")]
    floor_version: u64,
    /// The full snapshot the floor rests on. Pinned: retention never removes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    floor_snapshot: Option<String>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartMeta {
    seq: u64,
    file: String,
    first_version: u64,
    last_version: u64,
    records: u64,
    bytes: u64,
    sealed_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotMeta {
    snapshot_id: String,
    graph_version: u64,
    scope: Scope,
    nodes: u64,
    edges: u64,
    bytes: u64,
    created_at_ms: i64,
    full: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeadFile {
    #[serde(default = "legacy_format")]
    format: u32,
    graph_version: u64,
    sealed_version: u64,
    manifest_version: u64,
    last_write_ms: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveMeta {
    #[serde(default = "legacy_format")]
    format: u32,
    opened_at_ms: i64,
    seq: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceFile {
    #[serde(default = "legacy_format")]
    format: u32,
    instance_id: String,
    domain: String,
}

impl Store {
    pub fn open(cfg: StoreConfig) -> Result<Self, EhError> {
        if cfg.instance_id.is_empty() || cfg.domain.is_empty() {
            return Err(EhError::invalid("instance_id and domain are required"));
        }
        let identity = if cfg.claim_identity {
            Some(IdentityClaim::acquire(&cfg.instance_id, &cfg.domain)?)
        } else {
            None
        };
        let lock = DirLock::acquire(&cfg.data_dir)?;
        write_instance_file(&cfg)?;
        let namespaces = load_namespaces(&cfg)?;
        tracing::info!(
            instance_id = %cfg.instance_id,
            domain = %cfg.domain,
            namespaces = namespaces.len(),
            "ehgraph store opened"
        );
        Ok(Self {
            cfg,
            _lock: lock,
            _identity: identity,
            inner: Mutex::new(Inner { namespaces }),
        })
    }

    pub fn instance_id(&self) -> &str {
        &self.cfg.instance_id
    }

    pub fn domain(&self) -> &str {
        &self.cfg.domain
    }

    pub fn data_dir(&self) -> &Path {
        &self.cfg.data_dir
    }

    pub fn upsert(&self, request: WriteRequest) -> Result<WriteOutcome, EhError> {
        self.write_upsert(request, true)
    }

    /// Append and fsync a batch, then stop before the head pointer and memory publish.
    /// Simulates a crash after the log is durable and before acknowledgement.
    #[doc(hidden)]
    pub fn testing_append_unacked(&self, request: WriteRequest) -> Result<WriteOutcome, EhError> {
        self.write_upsert(request, false)
    }

    #[doc(hidden)]
    pub fn testing_append_raw(&self, namespace: &str, bytes: &[u8]) -> Result<(), EhError> {
        validate_namespace(namespace)?;
        let mut inner = self.lock()?;
        let ns = ensure_namespace(&self.cfg, &mut inner, namespace)?;
        append_raw(&self.cfg, ns, namespace, bytes)?;
        Ok(())
    }

    pub fn deactivate(&self, request: DeactivateRequest) -> Result<WriteOutcome, EhError> {
        validate_namespace(&request.namespace)?;
        if request.execution_id.is_empty() {
            return Err(EhError::invalid("execution_id is required"));
        }
        let hash = deactivate_hash(&request)?;
        let mut inner = self.lock()?;
        if !inner.namespaces.contains_key(&request.namespace) {
            return Err(EhError::not_found(format!(
                "namespace {} not found",
                request.namespace
            )));
        }
        if let Some(outcome) =
            take_idempotent(&inner, &request.namespace, &request.idempotency_key, &hash)?
        {
            return Ok(outcome);
        }
        let ns = inner.namespaces.get(&request.namespace).expect("checked");
        let ops = plan_deactivate(
            &ns.graph,
            &request.ids,
            request.origin.as_deref(),
            request.source.as_deref(),
            request.not_seen_since_ms,
            request.ts_ms,
        )?;
        let namespace = request.namespace.clone();
        let key = request.idempotency_key.clone();
        self.commit(
            &mut inner,
            &namespace,
            key,
            hash,
            request.execution_id,
            request.ts_ms,
            ops,
            true,
        )
    }

    pub fn summaries(&self) -> Result<Vec<NsSummary>, EhError> {
        let inner = self.lock()?;
        Ok(inner
            .namespaces
            .iter()
            .map(|(name, ns)| NsSummary {
                namespace: name.clone(),
                head_version: ns.head,
                sealed_version: ns.sealed_version,
                last_write_ms: ns.last_write_ms,
                nodes: ns.graph.nodes.len() as u64,
                edges: ns.graph.edges.len() as u64,
                idempotency_keys: ns.keys.len() as u64,
                idempotency_key_bytes: ns.keys.bytes(),
                idempotency_keys_evicted: ns.keys.evicted(),
                graph_bytes: ns.graph_bytes,
                log_parts: ns.manifest.parts.len() as u64,
                log_bytes: ns.manifest.parts.iter().map(|p| p.bytes).sum::<u64>() + ns.active_bytes,
                floor_version: ns.manifest.floor_version,
                replay_ms: ns.replay_ms,
            })
            .collect())
    }

    pub fn head(&self, namespace: &str) -> Result<u64, EhError> {
        let inner = self.lock()?;
        inner
            .namespaces
            .get(namespace)
            .map(|ns| ns.head)
            .ok_or_else(|| EhError::not_found(format!("namespace {namespace} not found")))
    }

    pub fn sealed_version(&self, namespace: &str) -> Result<u64, EhError> {
        let inner = self.lock()?;
        inner
            .namespaces
            .get(namespace)
            .map(|ns| ns.sealed_version)
            .ok_or_else(|| EhError::not_found(format!("namespace {namespace} not found")))
    }

    pub fn graph_at(
        &self,
        namespace: &str,
        version: Option<u64>,
    ) -> Result<MaterializedGraph, EhError> {
        let inner = self.lock()?;
        let ns = inner
            .namespaces
            .get(namespace)
            .ok_or_else(|| EhError::not_found(format!("namespace {namespace} not found")))?;
        let version = version.unwrap_or(ns.head);
        if version > ns.head {
            return Err(EhError::invalid(format!(
                "version {version} is past head {}",
                ns.head
            )));
        }
        if version == ns.head {
            return Ok(ns.graph.clone());
        }
        // Below the compaction floor only a retained full snapshot's exact version
        // can be read; the frames in between are gone (C02).
        if version < ns.manifest.floor_version
            && !ns
                .manifest
                .snapshots
                .iter()
                .any(|snap| snap.full && snap.graph_version == version)
        {
            return Err(EhError::not_found(format!(
                "version {version} of {namespace} is below the compaction floor {}; only \
                 retained full snapshots remain there",
                ns.manifest.floor_version
            )));
        }
        materialize_to(&self.cfg, namespace, &ns.manifest, version)
    }

    /// Run `f` on the live graph at head, under the store lock, without copying it.
    /// For point reads; keep `f` short, it blocks writers.
    pub fn with_head<T>(
        &self,
        namespace: &str,
        f: impl FnOnce(&MaterializedGraph) -> T,
    ) -> Result<T, EhError> {
        let inner = self.lock()?;
        let ns = inner
            .namespaces
            .get(namespace)
            .ok_or_else(|| EhError::not_found(format!("namespace {namespace} not found")))?;
        Ok(f(&ns.graph))
    }

    pub fn create_snapshot(&self, namespace: &str, scope: &Scope) -> Result<SnapshotInfo, EhError> {
        let mut inner = self.lock()?;
        let ns = inner
            .namespaces
            .get_mut(namespace)
            .ok_or_else(|| EhError::not_found(format!("namespace {namespace} not found")))?;
        if let Scope::Snapshot { snapshot_id } = scope {
            return snapshot_by_id(namespace, ns, snapshot_id);
        }
        let info = write_snapshot(&self.cfg, namespace, ns, scope)?;
        let retired = retain_snapshots(&self.cfg, namespace, &mut ns.manifest);
        write_manifest(&self.cfg, namespace, &ns.manifest)?;
        remove_dirs(retired);
        let _ = write_head_file(&self.cfg, namespace, ns);
        Ok(to_info(namespace, &info))
    }

    pub fn load_snapshot(
        &self,
        namespace: &str,
        snapshot_id: &str,
    ) -> Result<MaterializedGraph, EhError> {
        let inner = self.lock()?;
        let ns = inner
            .namespaces
            .get(namespace)
            .ok_or_else(|| EhError::not_found(format!("namespace {namespace} not found")))?;
        let meta = ns
            .manifest
            .snapshots
            .iter()
            .find(|snap| snap.snapshot_id == snapshot_id)
            .cloned()
            .ok_or_else(|| EhError::not_found(format!("snapshot {snapshot_id} not found")))?;
        load_snapshot_graph(&self.cfg, namespace, &meta)
    }

    pub fn version_info(
        &self,
        namespace: &str,
        version: Option<u64>,
    ) -> Result<VersionInfo, EhError> {
        let graph = self.graph_at(namespace, version)?;
        let inner = self.lock()?;
        let ns = inner
            .namespaces
            .get(namespace)
            .ok_or_else(|| EhError::not_found(format!("namespace {namespace} not found")))?;
        let snapshot = ns
            .manifest
            .snapshots
            .iter()
            .rev()
            .find(|snap| snap.graph_version == graph.version)
            .map(|snap| to_info(namespace, snap));
        let nodes = nodes_to_batch(&graph.node_list())?;
        let edges = edges_to_batch(&graph.edge_list())?;
        let bytes = crate::arrow_batch::batch_memory_size(&nodes)
            + crate::arrow_batch::batch_memory_size(&edges);
        Ok(VersionInfo {
            graph_version: graph.version,
            nodes: graph.nodes.len() as u64,
            edges: graph.edges.len() as u64,
            bytes,
            snapshot,
        })
    }

    /// Seal every open part so a backup can ship a complete prefix.
    pub fn seal_for_backup(&self) -> Result<(), EhError> {
        let mut inner = self.lock()?;
        let names: Vec<String> = inner.namespaces.keys().cloned().collect();
        for name in names {
            seal_namespace(
                &self.cfg,
                inner.namespaces.get_mut(&name).expect("name"),
                &name,
            )?;
        }
        Ok(())
    }

    pub fn prepare_backup(&self) -> Result<BackupPlan, EhError> {
        self.seal_for_backup()?;
        let inner = self.lock()?;
        let mut namespaces = Vec::new();
        for (name, ns) in &inner.namespaces {
            let mut files = Vec::new();
            for part in &ns.manifest.parts {
                let relative = PathBuf::from("namespaces").join(name).join(&part.file);
                files.push((self.cfg.data_dir.join(&relative), relative));
            }
            for snap in &ns.manifest.snapshots {
                for leaf in ["nodes.arrow", "edges.arrow", "meta.json"] {
                    let relative = PathBuf::from("namespaces")
                        .join(name)
                        .join("snapshots")
                        .join(&snap.snapshot_id)
                        .join(leaf);
                    files.push((self.cfg.data_dir.join(&relative), relative));
                }
            }
            let manifest_rel = PathBuf::from("namespaces").join(name).join("manifest.json");
            files.push((self.cfg.data_dir.join(&manifest_rel), manifest_rel));
            let head_relative = PathBuf::from("namespaces").join(name).join("head.json");
            let head = HeadFile {
                format: current_format(),
                graph_version: ns.sealed_version,
                sealed_version: ns.sealed_version,
                manifest_version: ns.manifest.version,
                last_write_ms: ns.last_write_ms,
            };
            namespaces.push(BackupNamespace {
                namespace: name.clone(),
                head_version: ns.head,
                sealed_version: ns.sealed_version,
                last_write_ms: ns.last_write_ms,
                files,
                head_relative,
                head_json: serde_json::to_string_pretty(&head)?,
            });
        }
        let mut schema_files = Vec::new();
        let schema_dir = self.cfg.data_dir.join("schemas");
        if schema_dir.is_dir() {
            for entry in fs::read_dir(&schema_dir)? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    let relative = PathBuf::from("schemas").join(entry.file_name());
                    schema_files.push((entry.path(), relative));
                }
            }
        }
        Ok(BackupPlan {
            instance_id: self.cfg.instance_id.clone(),
            domain: self.cfg.domain.clone(),
            namespaces,
            instance_file: self.cfg.data_dir.join("instance.json"),
            schema_files,
        })
    }

    fn write_upsert(&self, request: WriteRequest, publish: bool) -> Result<WriteOutcome, EhError> {
        validate_namespace(&request.namespace)?;
        if request.source.is_empty() || request.execution_id.is_empty() {
            return Err(EhError::invalid("source and execution_id are required"));
        }
        let hash = upsert_hash(&request)?;
        let mut inner = self.lock()?;
        if let Some(outcome) =
            take_idempotent(&inner, &request.namespace, &request.idempotency_key, &hash)?
        {
            return Ok(outcome);
        }
        let schema = read_schema(&self.cfg, &request.namespace)?;
        // Plan against the live graph by reference: cloning it here made every write
        // briefly hold a second copy of the whole namespace (C11).
        let empty = MaterializedGraph::default();
        let existing = inner
            .namespaces
            .get(&request.namespace)
            .map(|ns| &ns.graph)
            .unwrap_or(&empty);
        let ops = plan_upsert(existing, &request, &schema)?;
        let namespace = request.namespace.clone();
        let key = request.idempotency_key.clone();
        let execution_id = request.execution_id.clone();
        let ts_ms = request.ts_ms;
        self.commit(
            &mut inner,
            &namespace,
            key,
            hash,
            execution_id,
            ts_ms,
            ops,
            publish,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn commit(
        &self,
        inner: &mut Inner,
        namespace: &str,
        key: Option<String>,
        hash: String,
        execution_id: String,
        ts_ms: i64,
        mut ops: Vec<ehgraph_core::StoredOp>,
        publish: bool,
    ) -> Result<WriteOutcome, EhError> {
        if let Some(ref key) = key {
            if key.is_empty() || key.len() > 256 {
                return Err(EhError::invalid(
                    "idempotency_key must be 1..=256 characters",
                ));
            }
        }
        if ops.is_empty() {
            let head = inner
                .namespaces
                .get(namespace)
                .map(|ns| ns.head)
                .unwrap_or(0);
            if key.is_some() && head > 0 {
                let note = FrameBody::Idempotency(IdempotencyNote {
                    namespace: namespace.to_string(),
                    key: key.clone().expect("key"),
                    body_hash: hash,
                    graph_version: head,
                });
                let ns = ensure_namespace(&self.cfg, inner, namespace)?;
                let bytes = frame::encode(&note)?;
                maybe_seal(&self.cfg, ns, namespace)?;
                append_raw(&self.cfg, ns, namespace, &bytes)?;
                if publish {
                    if let FrameBody::Idempotency(note) = note {
                        ns.keys
                            .remember(&note.key, note.graph_version, &note.body_hash);
                    }
                    let _ = write_head_file(&self.cfg, namespace, ns);
                    maybe_seal(&self.cfg, ns, namespace)?;
                    write_derived_marker(&self.cfg, namespace, ns.head)?;
                }
            }
            return Ok(WriteOutcome {
                graph_version: head,
                changed: false,
                applied: 0,
            });
        }
        let delta = graph_bytes_delta(inner.namespaces.get(namespace).map(|ns| &ns.graph), &ops);
        if self.cfg.resident_graph_bytes > 0 && delta > 0 {
            let resident: u64 = inner.namespaces.values().map(|ns| ns.graph_bytes).sum();
            let after = resident.saturating_add(delta as u64);
            if after > self.cfg.resident_graph_bytes {
                return Err(EhError::new(
                    ErrorCode::MemoryBudgetExceeded,
                    format!(
                        "resident graph budget exceeded: this write needs {delta} more bytes; \
                         {resident} of {} are in use across namespaces. Deactivate or prune \
                         elements, or raise [budgets] resident_graph_bytes",
                        self.cfg.resident_graph_bytes
                    ),
                ));
            }
        }
        let ns = ensure_namespace(&self.cfg, inner, namespace)?;
        let version = ns.head + 1;
        stamp_ops(&mut ops, version);
        let applied = ops.len() as u64;
        let batch = ChangeBatch {
            namespace: namespace.to_string(),
            graph_version: version,
            idempotency_key: key,
            body_hash: hash,
            source: source_of(&ops),
            execution_id,
            ts_ms,
            ops,
        };
        let frame = FrameBody::Commit(batch);
        let bytes = frame::encode(&frame)?;
        maybe_seal(&self.cfg, ns, namespace)?;
        append_raw(&self.cfg, ns, namespace, &bytes)?;
        if !publish {
            tracing::warn!(
                instance_id = %self.cfg.instance_id,
                namespace,
                graph_version = version,
                "change log fsynced without publishing the head (crash window)"
            );
            return Ok(WriteOutcome {
                graph_version: version,
                changed: true,
                applied,
            });
        }
        if let FrameBody::Commit(batch) = &frame {
            absorb_frame(
                &mut ns.graph,
                &mut ns.keys,
                &FrameBody::Commit(batch.clone()),
                true,
            )?;
            ns.head = version;
            ns.last_write_ms = ts_ms;
            ns.graph_bytes = (ns.graph_bytes as i64).saturating_add(delta).max(0) as u64;
        }
        write_head_file(&self.cfg, namespace, ns)?;
        maybe_seal(&self.cfg, ns, namespace)?;
        write_derived_marker(&self.cfg, namespace, ns.head)?;
        tracing::info!(
            instance_id = %self.cfg.instance_id,
            domain = %self.cfg.domain,
            namespace,
            graph_version = version,
            "change log committed"
        );
        Ok(WriteOutcome {
            graph_version: version,
            changed: true,
            applied,
        })
    }

    /// Fold sealed change-log parts into a full snapshot and remove them (C02).
    ///
    /// A prefix of sealed parts (by seq) is dropped only when **every** part in it:
    ///
    /// 1. is covered by a full snapshot (one is written at head if none covers it);
    /// 2. has been shipped: `last_version <= shipped_version`, the backup
    ///    checkpoint's `backed_up_version`. Never-shipped parts stay;
    /// 3. records no idempotency key, or is older than the idempotency window: its
    ///    keys have all been forgotten already (C23), so replaying without it
    ///    rebuilds the same key window.
    ///
    /// and the prefix has at least `min_parts` parts. Crash safety: the snapshot is
    /// durable first, then the new manifest (with the floor), and only then are the
    /// part files deleted. A crash before the manifest leaves the old log in force; a
    /// crash after it leaves files that the next open removes. Open replays from the
    /// floor snapshot plus the remaining tail.
    pub fn compact(
        &self,
        namespace: &str,
        shipped_version: u64,
        min_parts: usize,
    ) -> Result<CompactionReport, EhError> {
        self.compact_inner(namespace, shipped_version, min_parts, None)
    }

    #[doc(hidden)]
    pub fn testing_compact_crash(
        &self,
        namespace: &str,
        shipped_version: u64,
        min_parts: usize,
        crash: CompactCrash,
    ) -> Result<CompactionReport, EhError> {
        self.compact_inner(namespace, shipped_version, min_parts, Some(crash))
    }

    fn compact_inner(
        &self,
        namespace: &str,
        shipped_version: u64,
        min_parts: usize,
        crash: Option<CompactCrash>,
    ) -> Result<CompactionReport, EhError> {
        let mut inner = self.lock()?;
        let ns = inner
            .namespaces
            .get_mut(namespace)
            .ok_or_else(|| EhError::not_found(format!("namespace {namespace} not found")))?;
        let mut report = CompactionReport {
            namespace: namespace.to_string(),
            dropped_parts: 0,
            dropped_bytes: 0,
            floor_version: ns.manifest.floor_version,
            floor_snapshot: ns.manifest.floor_snapshot.clone(),
            skipped: None,
        };
        let skip = |mut report: CompactionReport, why: String| {
            report.skipped = Some(why);
            Ok(report)
        };
        // A key recorded at log clock `c` is forgotten once the clock passes
        // `c + max_age`. A part whose frames all precede `key_floor` holds only
        // forgotten keys.
        let key_floor = match ns.keys.clock_ms() {
            Some(clock) => clock.saturating_sub(self.cfg.idempotency.max_age_ms),
            None => i64::MAX,
        };
        let mut parts = ns.manifest.parts.clone();
        parts.sort_by_key(|part| part.seq);
        let mut clock = i64::MIN;
        let mut candidates = Vec::new();
        for part in &parts {
            if part.last_version > shipped_version || part.last_version > ns.head {
                break;
            }
            let (max_ts, has_keys) = match ns.part_max_ts.get(&part.seq) {
                Some(info) => *info,
                None => {
                    let info = part_key_info(&self.cfg, namespace, part)?;
                    ns.part_max_ts.insert(part.seq, info);
                    info
                }
            };
            clock = clock.max(max_ts);
            // A part with no keys cannot change the key window, whatever its age.
            if has_keys && clock >= key_floor {
                break;
            }
            candidates.push(part.clone());
        }
        if candidates.len() < min_parts.max(1) {
            let why = format!(
                "{} of {} sealed parts are compactable (shipped to {shipped_version}, outside \
                 the idempotency window); need {}",
                candidates.len(),
                parts.len(),
                min_parts.max(1)
            );
            return skip(report, why);
        }
        let new_floor = candidates.iter().map(|p| p.last_version).max().unwrap_or(0);

        // 1. A durable full snapshot exactly at the floor, so every version at or
        //    above the floor stays readable. Materialized from the log while all of
        //    it is still there (a transient second copy of the graph).
        let existing = ns
            .manifest
            .snapshots
            .iter()
            .find(|snap| snap.full && snap.graph_version == new_floor)
            .map(|snap| snap.snapshot_id.clone());
        let snapshot_id = match existing {
            Some(id) => id,
            None if new_floor == ns.head => {
                write_snapshot(&self.cfg, namespace, ns, &Scope::Full)?.snapshot_id
            }
            None => {
                let at_floor = materialize_to(&self.cfg, namespace, &ns.manifest, new_floor)?;
                write_snapshot_of(
                    &self.cfg,
                    namespace,
                    &mut ns.manifest,
                    &at_floor,
                    &Scope::Full,
                )?
                .snapshot_id
            }
        };
        if crash == Some(CompactCrash::BeforeManifest) {
            return Ok(report);
        }

        // 2. The new manifest: parts gone, floor raised, floor snapshot pinned.
        let dropped: Vec<u64> = candidates.iter().map(|p| p.seq).collect();
        ns.manifest
            .parts
            .retain(|part| !dropped.contains(&part.seq));
        ns.manifest.floor_version = ns.manifest.floor_version.max(new_floor);
        ns.manifest.floor_snapshot = Some(snapshot_id.clone());
        ns.manifest.version += 1;
        let retired = retain_snapshots(&self.cfg, namespace, &mut ns.manifest);
        write_manifest(&self.cfg, namespace, &ns.manifest)?;
        write_head_file(&self.cfg, namespace, ns)?;
        if crash == Some(CompactCrash::AfterManifest) {
            return Ok(report);
        }

        // 3. Only now remove the folded parts.
        let dir = ns_dir(&self.cfg, namespace);
        for part in &candidates {
            fs::remove_file(dir.join(&part.file))?;
            ns.part_max_ts.remove(&part.seq);
            report.dropped_parts += 1;
            report.dropped_bytes += part.bytes;
        }
        fsync_dir(&dir.join("changelog"))?;
        remove_dirs(retired);
        report.floor_version = ns.manifest.floor_version;
        report.floor_snapshot = Some(snapshot_id);
        tracing::info!(
            instance_id = %self.cfg.instance_id,
            namespace,
            dropped_parts = report.dropped_parts,
            dropped_bytes = report.dropped_bytes,
            floor_version = report.floor_version,
            "compacted change log"
        );
        Ok(report)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, EhError> {
        self.inner
            .lock()
            .map_err(|_| EhError::unavailable("store lock poisoned"))
    }
}

fn source_of(ops: &[ehgraph_core::StoredOp]) -> String {
    match ops.first() {
        Some(ehgraph_core::StoredOp::UpsertNode { node }) => node.source.clone(),
        Some(ehgraph_core::StoredOp::UpsertEdge { edge }) => edge.source.clone(),
        _ => String::new(),
    }
}

fn take_idempotent(
    inner: &Inner,
    namespace: &str,
    key: &Option<String>,
    hash: &str,
) -> Result<Option<WriteOutcome>, EhError> {
    let Some(key) = key else {
        return Ok(None);
    };
    let Some(ns) = inner.namespaces.get(namespace) else {
        return Ok(None);
    };
    let Some(prev) = ns.keys.get(key) else {
        return Ok(None);
    };
    if prev.hash != hash {
        return Err(EhError::conflict(format!(
            "idempotency key {key} was already used with a different payload"
        )));
    }
    Ok(Some(WriteOutcome {
        graph_version: prev.version,
        changed: false,
        applied: 0,
    }))
}

fn upsert_hash(request: &WriteRequest) -> Result<String, EhError> {
    let value = serde_json::json!({
        "op": "upsert",
        "namespace": request.namespace,
        "nodes": request.nodes,
        "edges": request.edges,
        "source": request.source,
    });
    Ok(canonical_hash(&value))
}

fn deactivate_hash(request: &DeactivateRequest) -> Result<String, EhError> {
    let value = serde_json::json!({
        "op": "deactivate",
        "namespace": request.namespace,
        "ids": request.ids,
        "origin": request.origin,
        "source": request.source,
        "not_seen_since_ms": request.not_seen_since_ms,
    });
    Ok(canonical_hash(&value))
}

fn write_instance_file(cfg: &StoreConfig) -> Result<(), EhError> {
    let path = cfg.data_dir.join("instance.json");
    if path.exists() {
        let current: InstanceFile = decode_marked(&fs::read(&path)?, &path)?;
        if current.instance_id != cfg.instance_id || current.domain != cfg.domain {
            return Err(EhError::conflict(format!(
                "data directory belongs to {} (domain {}), not {} (domain {})",
                current.instance_id, current.domain, cfg.instance_id, cfg.domain
            )));
        }
        return Ok(());
    }
    let body = serde_json::to_vec_pretty(&InstanceFile {
        format: current_format(),
        instance_id: cfg.instance_id.clone(),
        domain: cfg.domain.clone(),
    })?;
    atomic_write(&path, &body)
}

fn load_namespaces(cfg: &StoreConfig) -> Result<BTreeMap<String, Namespace>, EhError> {
    let root = cfg.data_dir.join("namespaces");
    let mut out = BTreeMap::new();
    if !root.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        out.insert(name.clone(), recover_namespace(cfg, &name)?);
    }
    Ok(out)
}

fn recover_namespace(cfg: &StoreConfig, namespace: &str) -> Result<Namespace, EhError> {
    let started = Instant::now();
    let mut manifest = read_manifest(cfg, namespace)?;
    adopt_unlisted_parts(cfg, namespace, &mut manifest)?;
    let frames = read_frames(cfg, namespace, &manifest)?;
    // After compaction the log may hold nothing at or below the floor, so the floor
    // itself is a version the namespace is known to have reached.
    let log_version = frames
        .iter()
        .filter_map(|frame| match frame {
            FrameBody::Commit(batch) => Some(batch.graph_version),
            FrameBody::Idempotency(_) => None,
        })
        .max()
        .unwrap_or(0)
        .max(manifest.floor_version);
    let mut graph = MaterializedGraph::default();
    let mut keys = KeyWindow::new(cfg.idempotency);
    let base = manifest
        .snapshots
        .iter()
        .filter(|snap| snap.full && snap.graph_version <= log_version)
        .max_by_key(|snap| snap.graph_version)
        .cloned();
    if manifest.floor_version > 0 {
        let covered = base
            .as_ref()
            .is_some_and(|snap| snap.graph_version >= manifest.floor_version);
        if !covered {
            return Err(EhError::unavailable(format!(
                "namespace {namespace} is compacted to version {} but no full snapshot at or \
                 above it is present (floor snapshot {:?}); restore from backup",
                manifest.floor_version, manifest.floor_snapshot
            )));
        }
    }
    if let Some(snap) = &base {
        graph = load_snapshot_graph(cfg, namespace, snap)?;
    }
    for frame in &frames {
        let apply = match frame {
            FrameBody::Commit(batch) => batch.graph_version > graph.version,
            FrameBody::Idempotency(_) => false,
        };
        absorb_frame(&mut graph, &mut keys, frame, apply)?;
    }
    if graph.version != log_version {
        return Err(EhError::unavailable(format!(
            "namespace {namespace} replayed to version {} but the log head is {log_version}",
            graph.version
        )));
    }
    let head_file = read_head_file(cfg, namespace)?;
    if let Some(head) = &head_file {
        if head.graph_version > log_version {
            return Err(EhError::unavailable(format!(
                "namespace {namespace} head file is {} but the log only reaches {log_version}",
                head.graph_version
            )));
        }
    }
    let recorded_version = head_file.as_ref().map(|head| head.graph_version);
    let last_write_ms = head_file.map(|head| head.last_write_ms).unwrap_or(0);
    let (active_records, active_bytes, active_opened_ms, next_seq) =
        scan_active(cfg, namespace, &manifest)?;
    let sealed_version = manifest
        .parts
        .iter()
        .map(|part| part.last_version)
        .max()
        .unwrap_or(0);
    let graph_bytes = graph_resident_bytes(&graph);
    let replay_ms = started.elapsed().as_millis() as u64;
    let ns = Namespace {
        head: log_version,
        sealed_version,
        manifest,
        graph,
        graph_bytes,
        keys,
        active_records,
        active_bytes,
        active_opened_ms,
        next_seq,
        last_write_ms,
        replay_ms,
        part_max_ts: HashMap::new(),
    };
    if recorded_version != Some(log_version) {
        tracing::warn!(
            namespace,
            log_version,
            "fast-forwarding head pointer to the durable change log"
        );
        write_head_file(cfg, namespace, &ns)?;
    }
    Ok(ns)
}

fn adopt_unlisted_parts(
    cfg: &StoreConfig,
    namespace: &str,
    manifest: &mut Manifest,
) -> Result<(), EhError> {
    let dir = ns_dir(cfg, namespace).join("changelog");
    if !dir.exists() {
        return Ok(());
    }
    let mut extra = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".sealed") {
            continue;
        }
        let listed = manifest.parts.iter().any(|part| part.file.ends_with(&name));
        if !listed {
            extra.push(name);
        }
    }
    extra.sort();
    if extra.is_empty() {
        return Ok(());
    }
    for name in extra {
        let path = dir.join(&name);
        let bytes = fs::read(&path)?;
        let scan = decode_part(&bytes)?;
        if scan.torn {
            return Err(EhError::unavailable(format!(
                "sealed part {name} is truncated"
            )));
        }
        let seq = parse_part_seq(&name).unwrap_or(manifest.parts.len() as u64 + 1);
        let max_version = scan
            .frames
            .iter()
            .map(|frame| match frame {
                FrameBody::Commit(batch) => batch.graph_version,
                FrameBody::Idempotency(note) => note.graph_version,
            })
            .max()
            .unwrap_or(0);
        if manifest.floor_version > 0 && max_version <= manifest.floor_version {
            // Left behind by a compaction that crashed after its manifest was durable,
            // or restored from a backup prefix that still holds pre-compaction parts.
            // Everything in it is folded into the floor snapshot.
            fs::remove_file(&path)?;
            fsync_dir(&dir)?;
            tracing::info!(namespace, %name, floor = manifest.floor_version, "removed a compacted part left on disk");
            continue;
        }
        let versions: Vec<u64> = scan
            .frames
            .iter()
            .map(|frame| match frame {
                FrameBody::Commit(batch) => batch.graph_version,
                FrameBody::Idempotency(note) => note.graph_version,
            })
            .collect();
        manifest.parts.push(PartMeta {
            seq,
            file: format!("changelog/{name}"),
            first_version: versions.iter().copied().min().unwrap_or(0),
            last_version: versions.iter().copied().max().unwrap_or(0),
            records: scan.frames.len() as u64,
            bytes: bytes.len() as u64,
            sealed_at_ms: now_ms(),
        });
        manifest.version += 1;
        tracing::warn!(namespace, %name, "adopted a sealed part missing from the manifest");
    }
    write_manifest(cfg, namespace, manifest)?;
    Ok(())
}

fn parse_part_seq(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("part-")?.strip_suffix(".sealed")?;
    digits.parse().ok()
}

fn materialize_to(
    cfg: &StoreConfig,
    namespace: &str,
    manifest: &Manifest,
    target: u64,
) -> Result<MaterializedGraph, EhError> {
    let frames = read_frames(cfg, namespace, manifest)?;
    let base = manifest
        .snapshots
        .iter()
        .filter(|snap| snap.full && snap.graph_version <= target)
        .max_by_key(|snap| snap.graph_version)
        .cloned();
    let mut graph = match &base {
        Some(snap) => load_snapshot_graph(cfg, namespace, snap)?,
        None => MaterializedGraph::default(),
    };
    // Discarded: materializing an old version never answers an idempotent retry.
    let mut keys = KeyWindow::new(cfg.idempotency);
    for frame in &frames {
        let apply = match frame {
            FrameBody::Commit(batch) => {
                batch.graph_version > graph.version && batch.graph_version <= target
            }
            FrameBody::Idempotency(_) => false,
        };
        absorb_frame(&mut graph, &mut keys, frame, apply)?;
    }
    if graph.version != target {
        return Err(EhError::unavailable(format!(
            "could not materialize {namespace} at version {target} (reached {})",
            graph.version
        )));
    }
    Ok(graph)
}

fn load_snapshot_graph(
    cfg: &StoreConfig,
    namespace: &str,
    snap: &SnapshotMeta,
) -> Result<MaterializedGraph, EhError> {
    let dir = ns_dir(cfg, namespace)
        .join("snapshots")
        .join(&snap.snapshot_id);
    let nodes = ipc_to_nodes(&fs::read(dir.join("nodes.arrow"))?)?;
    let edges = ipc_to_edges(&fs::read(dir.join("edges.arrow"))?)?;
    Ok(MaterializedGraph::from_elements(
        snap.graph_version,
        nodes,
        edges,
    ))
}

fn read_frames(
    cfg: &StoreConfig,
    namespace: &str,
    manifest: &Manifest,
) -> Result<Vec<FrameBody>, EhError> {
    let mut frames = Vec::new();
    let mut parts = manifest.parts.clone();
    parts.sort_by_key(|part| part.seq);
    for part in parts {
        let path = ns_dir(cfg, namespace).join(&part.file);
        let bytes = fs::read(&path).map_err(|err| {
            EhError::unavailable(format!("missing sealed part {}: {err}", part.file))
        })?;
        let scan = decode_part(&bytes)?;
        if scan.torn {
            return Err(EhError::unavailable(format!(
                "sealed part {} is truncated",
                part.file
            )));
        }
        frames.extend(scan.frames);
    }
    let active = ns_dir(cfg, namespace).join("changelog").join("active.part");
    if active.exists() {
        let bytes = fs::read(&active)?;
        let scan = decode_part(&bytes)?;
        if scan.torn {
            let file = OpenOptions::new().write(true).open(&active)?;
            file.set_len(scan.good_len as u64)?;
            file.sync_all()?;
            tracing::warn!(
                namespace,
                dropped = bytes.len() - scan.good_len,
                "truncated a torn change-log tail"
            );
        }
        frames.extend(scan.frames);
    }
    Ok(frames)
}

fn scan_active(
    cfg: &StoreConfig,
    namespace: &str,
    manifest: &Manifest,
) -> Result<(u64, u64, i64, u64), EhError> {
    let dir = ns_dir(cfg, namespace).join("changelog");
    fs::create_dir_all(&dir)?;
    let active = dir.join("active.part");
    if !active.exists() {
        File::create(&active)?.sync_all()?;
    }
    let bytes = fs::read(&active)?;
    let scan = decode_part(&bytes)?;
    let next_from_manifest = manifest
        .parts
        .iter()
        .map(|part| part.seq)
        .max()
        .unwrap_or(0)
        + 1;
    let meta_path = dir.join("active.meta.json");
    let opened = if meta_path.exists() {
        let meta: ActiveMeta = decode_marked(&fs::read(&meta_path)?, &meta_path)?;
        meta.opened_at_ms
    } else {
        now_ms()
    };
    Ok((
        scan.frames.len() as u64,
        scan.good_len as u64,
        opened,
        next_from_manifest,
    ))
}

fn ensure_namespace<'a>(
    cfg: &StoreConfig,
    inner: &'a mut Inner,
    namespace: &str,
) -> Result<&'a mut Namespace, EhError> {
    if !inner.namespaces.contains_key(namespace) {
        let dir = ns_dir(cfg, namespace);
        fs::create_dir_all(dir.join("changelog"))?;
        fs::create_dir_all(dir.join("snapshots"))?;
        fs::create_dir_all(dir.join("derived"))?;
        let manifest = Manifest {
            format: current_format(),
            version: 0,
            namespace: namespace.to_string(),
            parts: Vec::new(),
            snapshots: Vec::new(),
            floor_version: 0,
            floor_snapshot: None,
        };
        write_manifest(cfg, namespace, &manifest)?;
        let active = dir.join("changelog").join("active.part");
        File::create(&active)?.sync_all()?;
        let opened = now_ms();
        atomic_write(
            &dir.join("changelog").join("active.meta.json"),
            &serde_json::to_vec(&ActiveMeta {
                format: current_format(),
                opened_at_ms: opened,
                seq: 1,
            })?,
        )?;
        let ns = Namespace {
            head: 0,
            sealed_version: 0,
            manifest,
            graph: MaterializedGraph::default(),
            graph_bytes: 0,
            keys: KeyWindow::new(cfg.idempotency),
            active_records: 0,
            active_bytes: 0,
            active_opened_ms: opened,
            next_seq: 1,
            last_write_ms: 0,
            replay_ms: 0,
            part_max_ts: HashMap::new(),
        };
        write_head_file(cfg, namespace, &ns)?;
        inner.namespaces.insert(namespace.to_string(), ns);
    }
    Ok(inner.namespaces.get_mut(namespace).expect("inserted"))
}

fn maybe_seal(cfg: &StoreConfig, ns: &mut Namespace, namespace: &str) -> Result<(), EhError> {
    if should_seal(cfg, ns) {
        seal_namespace(cfg, ns, namespace)?;
    }
    Ok(())
}

fn should_seal(cfg: &StoreConfig, ns: &Namespace) -> bool {
    if ns.active_records == 0 {
        return false;
    }
    let age_ms = cfg.seal_max_age.as_millis() as i64;
    ns.active_records >= cfg.seal_max_records
        || ns.active_bytes >= cfg.seal_max_bytes
        || now_ms().saturating_sub(ns.active_opened_ms) >= age_ms
}

fn seal_namespace(cfg: &StoreConfig, ns: &mut Namespace, namespace: &str) -> Result<(), EhError> {
    if ns.active_records == 0 {
        return Ok(());
    }
    let seq = ns.next_seq;
    let file_name = format!("part-{seq:08}.sealed");
    let relative = format!("changelog/{file_name}");
    let dir = ns_dir(cfg, namespace).join("changelog");
    let src = dir.join("active.part");
    let dest = dir.join(&file_name);
    fs::rename(&src, &dest)?;
    fsync_dir(&dir)?;
    let bytes = fs::read(&dest)?;
    let scan = decode_part(&bytes)?;
    if scan.torn {
        return Err(EhError::unavailable(
            "refusing to seal a truncated active part",
        ));
    }
    let versions: Vec<u64> = scan
        .frames
        .iter()
        .map(|frame| match frame {
            FrameBody::Commit(batch) => batch.graph_version,
            FrameBody::Idempotency(note) => note.graph_version,
        })
        .collect();
    let last_version = versions.iter().copied().max().unwrap_or(ns.sealed_version);
    ns.manifest.parts.push(PartMeta {
        seq,
        file: relative,
        first_version: versions.iter().copied().min().unwrap_or(0),
        last_version,
        records: scan.frames.len() as u64,
        bytes: bytes.len() as u64,
        sealed_at_ms: now_ms(),
    });
    ns.manifest.version += 1;
    ns.sealed_version = ns.sealed_version.max(last_version);
    ns.next_seq += 1;
    ns.active_records = 0;
    ns.active_bytes = 0;
    ns.active_opened_ms = now_ms();
    File::create(&src)?.sync_all()?;
    atomic_write(
        &dir.join("active.meta.json"),
        &serde_json::to_vec(&ActiveMeta {
            format: current_format(),
            opened_at_ms: ns.active_opened_ms,
            seq: ns.next_seq,
        })?,
    )?;
    write_manifest(cfg, namespace, &ns.manifest)?;
    write_head_file(cfg, namespace, ns)?;
    tracing::info!(
        namespace,
        seq,
        sealed_version = ns.sealed_version,
        "sealed change-log part"
    );
    Ok(())
}

fn append_raw(
    cfg: &StoreConfig,
    ns: &mut Namespace,
    namespace: &str,
    bytes: &[u8],
) -> Result<(), EhError> {
    let path = ns_dir(cfg, namespace).join("changelog").join("active.part");
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    ns.active_records += 1;
    ns.active_bytes += bytes.len() as u64;
    Ok(())
}

/// Drop the oldest snapshots beyond `snapshot_keep` from the manifest, never the
/// compaction floor's. Returns their directories, for the caller to delete **after**
/// the new manifest is durable (so a crash never leaves a manifest naming a deleted
/// snapshot).
fn retain_snapshots(cfg: &StoreConfig, namespace: &str, manifest: &mut Manifest) -> Vec<PathBuf> {
    let keep = cfg.snapshot_keep.max(1);
    let mut removed = Vec::new();
    while manifest.snapshots.len() > keep {
        let Some(index) = manifest
            .snapshots
            .iter()
            .position(|snap| Some(&snap.snapshot_id) != manifest.floor_snapshot.as_ref())
        else {
            break;
        };
        let snap = manifest.snapshots.remove(index);
        removed.push(
            ns_dir(cfg, namespace)
                .join("snapshots")
                .join(&snap.snapshot_id),
        );
    }
    removed
}

fn remove_dirs(dirs: Vec<PathBuf>) {
    for dir in dirs {
        if let Err(err) = fs::remove_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), %err, "could not remove a retired snapshot");
        }
    }
}

/// Write a snapshot of `ns` at its head and add it to the manifest (not yet written).
fn write_snapshot(
    cfg: &StoreConfig,
    namespace: &str,
    ns: &mut Namespace,
    scope: &Scope,
) -> Result<SnapshotMeta, EhError> {
    write_snapshot_of(cfg, namespace, &mut ns.manifest, &ns.graph, scope)
}

/// Write a snapshot of `graph` (at `graph.version`) and add it to `manifest`.
fn write_snapshot_of(
    cfg: &StoreConfig,
    namespace: &str,
    manifest: &mut Manifest,
    graph: &MaterializedGraph,
    scope: &Scope,
) -> Result<SnapshotMeta, EhError> {
    let scoped = graph.apply_scope(scope)?;
    let snapshot_id = format!("snap-{}-{}", graph.version, manifest.version + 1);
    let node_bytes = batches_to_ipc(&nodes_to_batch(&scoped.node_list())?)?;
    let edge_bytes = batches_to_ipc(&edges_to_batch(&scoped.edge_list())?)?;
    let dir = ns_dir(cfg, namespace).join("snapshots").join(&snapshot_id);
    atomic_write(&dir.join("nodes.arrow"), &node_bytes)?;
    atomic_write(&dir.join("edges.arrow"), &edge_bytes)?;
    let info = SnapshotMeta {
        snapshot_id,
        graph_version: graph.version,
        scope: scope.clone(),
        nodes: scoped.nodes.len() as u64,
        edges: scoped.edges.len() as u64,
        bytes: (node_bytes.len() + edge_bytes.len()) as u64,
        created_at_ms: now_ms(),
        full: matches!(scope, Scope::Full),
    };
    atomic_write(&dir.join("meta.json"), &serde_json::to_vec_pretty(&info)?)?;
    manifest.snapshots.push(info.clone());
    manifest.version += 1;
    Ok(info)
}

fn snapshot_by_id(
    namespace: &str,
    ns: &Namespace,
    snapshot_id: &str,
) -> Result<SnapshotInfo, EhError> {
    ns.manifest
        .snapshots
        .iter()
        .find(|snap| snap.snapshot_id == snapshot_id)
        .map(|snap| to_info(namespace, snap))
        .ok_or_else(|| EhError::not_found(format!("snapshot {snapshot_id} not found")))
}

fn to_info(namespace: &str, snap: &SnapshotMeta) -> SnapshotInfo {
    SnapshotInfo {
        snapshot_id: snap.snapshot_id.clone(),
        namespace: namespace.to_string(),
        graph_version: snap.graph_version,
        nodes: snap.nodes,
        edges: snap.edges,
        bytes: snap.bytes,
        created_at_ms: snap.created_at_ms,
        scope: snap.scope.clone(),
        full: snap.full,
    }
}

fn read_schema(cfg: &StoreConfig, namespace: &str) -> Result<SchemaConfig, EhError> {
    let path = cfg
        .data_dir
        .join("schemas")
        .join(format!("{namespace}.json"));
    if !path.exists() {
        return Ok(SchemaConfig::default());
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn read_manifest(cfg: &StoreConfig, namespace: &str) -> Result<Manifest, EhError> {
    let path = ns_dir(cfg, namespace).join("manifest.json");
    if !path.exists() {
        return Ok(Manifest {
            format: current_format(),
            version: 0,
            namespace: namespace.to_string(),
            parts: Vec::new(),
            snapshots: Vec::new(),
            floor_version: 0,
            floor_snapshot: None,
        });
    }
    decode_marked(&fs::read(&path)?, &path)
}

fn write_manifest(cfg: &StoreConfig, namespace: &str, manifest: &Manifest) -> Result<(), EhError> {
    // The lowest format that describes it: only a compacted manifest is format 2, so
    // an older build can open the directory until compaction has run (E9).
    let mut out = manifest.clone();
    out.format = if out.floor_version > 0 {
        crate::format::COMPACTED_MANIFEST_FORMAT
    } else {
        current_format()
    };
    atomic_write(
        &ns_dir(cfg, namespace).join("manifest.json"),
        &serde_json::to_vec_pretty(&out)?,
    )
}

fn read_head_file(cfg: &StoreConfig, namespace: &str) -> Result<Option<HeadFile>, EhError> {
    let path = ns_dir(cfg, namespace).join("head.json");
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(decode_marked(&fs::read(&path)?, &path)?))
}

fn write_head_file(cfg: &StoreConfig, namespace: &str, ns: &Namespace) -> Result<(), EhError> {
    let head = HeadFile {
        format: current_format(),
        graph_version: ns.head,
        sealed_version: ns.sealed_version,
        manifest_version: ns.manifest.version,
        last_write_ms: ns.last_write_ms,
    };
    atomic_write(
        &ns_dir(cfg, namespace).join("head.json"),
        &serde_json::to_vec_pretty(&head)?,
    )
}

fn write_derived_marker(cfg: &StoreConfig, namespace: &str, version: u64) -> Result<(), EhError> {
    let body = serde_json::json!({
        "graph_version": version,
        "derived": true,
        "note": "Safe to delete. Open rebuilds this namespace from the change log and full snapshots."
    });
    // Never read back (open rebuilds from the log), so it needs no fsync: a crash may
    // lose or truncate it and nothing cares. Skipping the two fsyncs here is most of a
    // small commit's latency on a flush-heavy disk (C17).
    let path = ns_dir(cfg, namespace).join("derived").join("cache.json");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&body)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn ns_dir(cfg: &StoreConfig, namespace: &str) -> PathBuf {
    cfg.data_dir.join("namespaces").join(namespace)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), EhError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

fn fsync_dir(path: &Path) -> Result<(), EhError> {
    let file = File::open(path)?;
    file.sync_all()?;
    Ok(())
}

/// Estimated resident bytes of a whole materialized graph.
fn graph_resident_bytes(graph: &MaterializedGraph) -> u64 {
    graph.nodes.values().map(|n| n.approx_bytes()).sum::<u64>()
        + graph.edges.values().map(|e| e.approx_bytes()).sum::<u64>()
}

/// How much applying `ops` changes the estimated resident bytes of `graph`.
/// Upserts replace whole images; a deactivation keeps the image (`active = false`).
fn graph_bytes_delta(graph: Option<&MaterializedGraph>, ops: &[StoredOp]) -> i64 {
    let mut delta = 0i64;
    for op in ops {
        match op {
            StoredOp::UpsertNode { node } => {
                let old = graph
                    .and_then(|g| g.nodes.get(&node.id))
                    .map(|n| n.approx_bytes())
                    .unwrap_or(0);
                delta += node.approx_bytes() as i64 - old as i64;
            }
            StoredOp::UpsertEdge { edge } => {
                let old = graph
                    .and_then(|g| g.edges.get(&edge.id))
                    .map(|e| e.approx_bytes())
                    .unwrap_or(0);
                delta += edge.approx_bytes() as i64 - old as i64;
            }
            StoredOp::DeactivateNode { .. } | StoredOp::DeactivateEdge { .. } => {}
        }
    }
    delta
}

/// One sealed part's highest commit `ts_ms` (`i64::MIN` when it has no commit), and
/// whether any of its frames records an idempotency key.
fn part_key_info(
    cfg: &StoreConfig,
    namespace: &str,
    part: &PartMeta,
) -> Result<(i64, bool), EhError> {
    let bytes = fs::read(ns_dir(cfg, namespace).join(&part.file))?;
    let scan = decode_part(&bytes)?;
    let mut max_ts = i64::MIN;
    let mut has_keys = false;
    for frame in &scan.frames {
        match frame {
            FrameBody::Commit(batch) => {
                max_ts = max_ts.max(batch.ts_ms);
                has_keys |= batch.idempotency_key.is_some();
            }
            FrameBody::Idempotency(_) => has_keys = true,
        }
    }
    Ok((max_ts, has_keys))
}
