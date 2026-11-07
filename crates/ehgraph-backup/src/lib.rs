//! Backup and restore for one ehgraph instance.
//!
//! A backup ships through an [`ObjectStore`]: a local directory ([`FsStore`]) or a
//! Google Cloud Storage prefix ([`gcs::GcsStore`], register C19). The target comes
//! from `backup_prefix`: `gs://bucket/prefix` is GCS; anything else is a directory.
//!
//! The ordering is the same on every target (see [`object`] for the per-object
//! rules):
//!
//! 1. immutable objects (`instance.json`, schema files, sealed parts, snapshots),
//!    put-if-absent;
//! 2. per namespace, the manifest, then the head pointer;
//! 3. `checkpoint.json`, last.
//!
//! A crash at any point leaves the previous checkpoint naming objects that all
//! exist. Restore reads and format-checks (E9) the checkpoint before it writes
//! anything.

pub mod gcs;
pub mod object;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ehgraph_core::EhError;
use ehgraph_store::{BackupPlan, Store};
use serde::{Deserialize, Serialize};

pub use gcs::{EnvToken, GcsLocation, GcsStore, MetadataToken, NoAuth, TokenProvider};
pub use object::{key_for, FsStore, ObjectStore};

pub const CHECKPOINT_KEY: &str = "checkpoint.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NamespaceCheckpoint {
    pub backed_up_version: u64,
    pub head_version_at_ship: u64,
    pub shipped_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    /// On-disk format marker (E9). A restore by an older build refuses a newer one.
    #[serde(default = "ehgraph_store::format::legacy_format")]
    pub format: u32,
    pub instance_id: String,
    pub domain: String,
    pub shipped_at_ms: i64,
    pub namespaces: BTreeMap<String, NamespaceCheckpoint>,
}

/// How to reach a non-directory backup target. The `[backup]` config section.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    /// GCS JSON API endpoint. Override only for an emulator.
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    /// `metadata` (GKE Workload Identity, the default), `env:VAR` (a bearer token in
    /// an environment variable), or `none` (an emulator).
    #[serde(default = "default_auth")]
    pub auth: String,
    /// Per-request timeout.
    #[serde(default = "default_timeout_s")]
    pub timeout_s: u64,
}

fn default_endpoint() -> String {
    gcs::DEFAULT_ENDPOINT.to_string()
}

fn default_auth() -> String {
    "metadata".to_string()
}

fn default_timeout_s() -> u64 {
    60
}

impl Default for TargetConfig {
    fn default() -> Self {
        Self {
            endpoint: default_endpoint(),
            auth: default_auth(),
            timeout_s: default_timeout_s(),
        }
    }
}

impl TargetConfig {
    /// The token provider `auth` names.
    pub fn token_provider(&self) -> Result<Arc<dyn TokenProvider>, EhError> {
        match self.auth.as_str() {
            "metadata" => Ok(Arc::new(MetadataToken::default())),
            "none" => Ok(Arc::new(NoAuth)),
            other => match other.strip_prefix("env:") {
                Some(var) if !var.is_empty() => Ok(Arc::new(EnvToken(var.to_string()))),
                _ => Err(EhError::invalid(format!(
                    "[backup] auth = {other:?}: expected \"metadata\", \"env:VAR\", or \"none\""
                ))),
            },
        }
    }
}

/// Open the target `backup_prefix` names. `token` overrides `[backup] auth` (a
/// host sharing its own credentials).
pub fn open_target(
    backup_prefix: &str,
    config: &TargetConfig,
    token: Option<Arc<dyn TokenProvider>>,
) -> Result<Arc<dyn ObjectStore>, EhError> {
    if backup_prefix.starts_with("gs://") {
        let location = GcsLocation::parse(backup_prefix)?;
        let token = match token {
            Some(token) => token,
            None => config.token_provider()?,
        };
        return Ok(Arc::new(GcsStore::new(
            location,
            &config.endpoint,
            Duration::from_secs(config.timeout_s.max(1)),
            token,
        )));
    }
    if let Some((scheme, _)) = backup_prefix.split_once("://") {
        return Err(EhError::invalid(format!(
            "backup_prefix {backup_prefix:?}: {scheme}:// is not supported; use gs://bucket/prefix \
             or a directory"
        )));
    }
    if backup_prefix.trim().is_empty() {
        return Err(EhError::invalid("backup_prefix is empty"));
    }
    Ok(Arc::new(FsStore::new(backup_prefix)))
}

#[derive(Debug)]
enum Cached {
    Unknown,
    Known(Option<Checkpoint>),
}

/// One instance's backup target, with the last checkpoint cached so metrics and
/// `graph.backup.status` do not read the bucket on every call.
#[derive(Debug)]
pub struct Backup {
    target: Arc<dyn ObjectStore>,
    cached: Mutex<Cached>,
    /// One ship at a time per instance (the scheduler and the admin route can race).
    shipping: Mutex<()>,
    failures: AtomicU64,
}

impl Backup {
    pub fn new(target: Arc<dyn ObjectStore>) -> Self {
        Self {
            target,
            cached: Mutex::new(Cached::Unknown),
            shipping: Mutex::new(()),
            failures: AtomicU64::new(0),
        }
    }

    pub fn target(&self) -> &Arc<dyn ObjectStore> {
        &self.target
    }

    pub fn describe(&self) -> String {
        self.target.describe()
    }

    /// The last checkpoint: from the cache, or read from the target once.
    pub fn checkpoint(&self) -> Result<Option<Checkpoint>, EhError> {
        if let Cached::Known(known) = &*self.cache() {
            return Ok(known.clone());
        }
        let read = read_checkpoint_from(self.target.as_ref())?;
        *self.cache() = Cached::Known(read.clone());
        Ok(read)
    }

    /// The cached checkpoint without any I/O. `None` when nothing is known yet.
    pub fn cached_checkpoint(&self) -> Option<Checkpoint> {
        match &*self.cache() {
            Cached::Known(known) => known.clone(),
            Cached::Unknown => None,
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Cached> {
        self.cached.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Ships that returned an error since this handle was created.
    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    /// Seal and ship: immutable objects, then manifest and head per namespace, then
    /// the checkpoint.
    pub fn ship(&self, store: &Store, shipped_at_ms: i64) -> Result<Checkpoint, EhError> {
        let _one = self.shipping.lock().unwrap_or_else(|p| p.into_inner());
        let result = self.ship_locked(store, shipped_at_ms);
        if result.is_err() {
            self.failures.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    fn ship_locked(&self, store: &Store, shipped_at_ms: i64) -> Result<Checkpoint, EhError> {
        let plan = store.prepare_backup()?;
        if plan.instance_id != store.instance_id() {
            return Err(EhError::unavailable("backup plan identity mismatch"));
        }
        let target = self.target.as_ref();
        let mut written = 0usize;
        written += target.put_file_if_absent("instance.json", &plan.instance_file)? as usize;
        for (src, relative) in &plan.schema_files {
            written += target.put_file_if_absent(&key_for(relative), src)? as usize;
        }
        let mut namespaces = BTreeMap::new();
        for ns in &plan.namespaces {
            let mut manifests = Vec::new();
            for (src, relative) in &ns.files {
                if relative.ends_with("manifest.json") {
                    manifests.push((src, relative));
                } else {
                    written += target.put_file_if_absent(&key_for(relative), src)? as usize;
                }
            }
            // The manifest names the parts above, so it follows them.
            for (src, relative) in manifests {
                target.put(&key_for(relative), &fs::read(src)?)?;
            }
            // Head is written after the immutable objects it names.
            target.put(&key_for(&ns.head_relative), ns.head_json.as_bytes())?;
            namespaces.insert(
                ns.namespace.clone(),
                NamespaceCheckpoint {
                    backed_up_version: ns.sealed_version,
                    head_version_at_ship: ns.head_version,
                    shipped_at_ms,
                },
            );
        }
        let checkpoint = Checkpoint {
            format: ehgraph_store::format::current_format(),
            instance_id: plan.instance_id,
            domain: plan.domain,
            shipped_at_ms,
            namespaces,
        };
        // Last object: a reader that trusts the checkpoint never sees a version whose
        // objects are missing.
        target.put(CHECKPOINT_KEY, &serde_json::to_vec_pretty(&checkpoint)?)?;
        *self.cache() = Cached::Known(Some(checkpoint.clone()));
        tracing::info!(
            instance_id = %checkpoint.instance_id,
            domain = %checkpoint.domain,
            namespaces = checkpoint.namespaces.len(),
            new_objects = written,
            target = %target.describe(),
            "backup shipped"
        );
        Ok(checkpoint)
    }

    /// Restore into an empty data directory. The caller then opens the store,
    /// which rebuilds from what was restored.
    pub fn restore(&self, data_dir: &Path) -> Result<Checkpoint, EhError> {
        restore_from(self.target.as_ref(), data_dir)
    }
}

fn read_checkpoint_from(target: &dyn ObjectStore) -> Result<Option<Checkpoint>, EhError> {
    let Some(bytes) = target.get(CHECKPOINT_KEY)? else {
        return Ok(None);
    };
    let label = checkpoint_label(target);
    Ok(Some(ehgraph_store::format::decode_marked(
        &bytes,
        Path::new(&label),
    )?))
}

fn checkpoint_label(target: &dyn ObjectStore) -> String {
    format!(
        "{}/{CHECKPOINT_KEY}",
        target.describe().trim_end_matches('/')
    )
}

fn restore_from(target: &dyn ObjectStore, data_dir: &Path) -> Result<Checkpoint, EhError> {
    // Read (and format-check, E9) the checkpoint BEFORE writing anything, so an older
    // build refuses a newer backup without leaving a half-restored directory.
    let Some(checkpoint) = read_checkpoint_from(target)? else {
        return Err(EhError::not_found(format!(
            "no checkpoint in {}",
            target.describe()
        )));
    };
    if data_dir.exists() && fs::read_dir(data_dir)?.next().is_some() {
        return Err(EhError::conflict(format!(
            "restore target {} is not empty",
            data_dir.display()
        )));
    }
    fs::create_dir_all(data_dir)?;
    let mut restored = 0usize;
    for key in target.list()? {
        if key == CHECKPOINT_KEY {
            continue;
        }
        object::check_key(&key)?;
        let Some(bytes) = target.get(&key)? else {
            // Listed then gone: only a concurrent delete does this, and nothing
            // ehgraph runs deletes from a backup prefix.
            return Err(EhError::unavailable(format!(
                "{key} was listed in {} but could not be read",
                target.describe()
            )));
        };
        object::atomic_write(&data_dir.join(&key), &bytes)?;
        restored += 1;
    }
    tracing::info!(
        instance_id = %checkpoint.instance_id,
        domain = %checkpoint.domain,
        objects = restored,
        source = %target.describe(),
        "backup restored into empty data dir"
    );
    Ok(checkpoint)
}

/// [`Backup::ship`] to a directory prefix.
pub fn ship(
    store: &Store,
    backup_prefix: &Path,
    shipped_at_ms: i64,
) -> Result<Checkpoint, EhError> {
    fs::create_dir_all(backup_prefix)?;
    Backup::new(Arc::new(FsStore::new(backup_prefix))).ship(store, shipped_at_ms)
}

/// The checkpoint in a directory prefix, if any.
pub fn read_checkpoint(backup_prefix: &Path) -> Result<Option<Checkpoint>, EhError> {
    let path = backup_prefix.join(CHECKPOINT_KEY);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(ehgraph_store::format::decode_marked(
        &fs::read(&path)?,
        &path,
    )?))
}

/// Restore a directory prefix into an empty data directory.
pub fn restore(backup_prefix: &Path, data_dir: &Path) -> Result<Checkpoint, EhError> {
    restore_from(&FsStore::new(backup_prefix), data_dir)
}

/// Lag of the live head behind the last shipped version.
pub fn lag(head_version: u64, checkpoint: Option<&NamespaceCheckpoint>, now_ms: i64) -> (u64, i64) {
    let Some(checkpoint) = checkpoint else {
        if head_version == 0 {
            return (0, 0);
        }
        return (head_version, 0);
    };
    let versions = head_version.saturating_sub(checkpoint.backed_up_version);
    let seconds = if versions == 0 {
        0
    } else {
        now_ms.saturating_sub(checkpoint.shipped_at_ms) / 1000
    };
    (versions, seconds.max(0))
}

/// Files a plan will ship, used by tests that want the relative names.
pub fn planned_relative_paths(plan: &BackupPlan) -> Vec<PathBuf> {
    plan.namespaces
        .iter()
        .flat_map(|ns| {
            ns.files
                .iter()
                .map(|(_, rel)| rel.clone())
                .chain(std::iter::once(ns.head_relative.clone()))
        })
        .collect()
}
