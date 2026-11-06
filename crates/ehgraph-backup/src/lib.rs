//! Backup and restore for one ehgraph instance.
//!
//! The destination is a directory prefix (`put_if_absent` for immutable parts,
//! head pointer last). It stands in for an object store until an S3-compatible
//! client is chosen.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use ehgraph_core::EhError;
use ehgraph_store::{BackupPlan, Store};
use serde::{Deserialize, Serialize};

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
    pub namespaces: std::collections::BTreeMap<String, NamespaceCheckpoint>,
}

/// Copy sealed parts, snapshots and the manifest, then the head pointer, then the checkpoint.
pub fn ship(
    store: &Store,
    backup_prefix: &Path,
    shipped_at_ms: i64,
) -> Result<Checkpoint, EhError> {
    let plan = store.prepare_backup()?;
    if plan.instance_id != store.instance_id() {
        return Err(EhError::unavailable("backup plan identity mismatch"));
    }
    fs::create_dir_all(backup_prefix)?;
    copy_if_absent(&plan.instance_file, &backup_prefix.join("instance.json"))?;
    for (src, relative) in &plan.schema_files {
        copy_if_absent(src, &backup_prefix.join(relative))?;
    }
    let mut namespaces = std::collections::BTreeMap::new();
    for ns in &plan.namespaces {
        for (src, relative) in &ns.files {
            let dest = backup_prefix.join(relative);
            if relative.ends_with("manifest.json") {
                atomic_write(&dest, &fs::read(src)?)?;
            } else {
                copy_if_absent(src, &dest)?;
            }
        }
        // Head is written after the immutable objects it names.
        atomic_write(
            &backup_prefix.join(&ns.head_relative),
            ns.head_json.as_bytes(),
        )?;
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
    // The checkpoint is the last object, so a reader never observes a head without it
    // only if they look at the checkpoint. Head files are already last per namespace.
    atomic_write(
        &backup_prefix.join("checkpoint.json"),
        &serde_json::to_vec_pretty(&checkpoint)?,
    )?;
    tracing::info!(
        instance_id = %checkpoint.instance_id,
        domain = %checkpoint.domain,
        namespaces = checkpoint.namespaces.len(),
        "backup shipped"
    );
    Ok(checkpoint)
}

pub fn read_checkpoint(backup_prefix: &Path) -> Result<Option<Checkpoint>, EhError> {
    let path = backup_prefix.join("checkpoint.json");
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(ehgraph_store::format::decode_marked(
        &fs::read(&path)?,
        &path,
    )?))
}

/// Restore into an empty data directory. The caller then opens the store, which rebuilds.
pub fn restore(backup_prefix: &Path, data_dir: &Path) -> Result<Checkpoint, EhError> {
    // Read (and format-check, E9) the checkpoint BEFORE copying anything, so an older
    // build refuses a newer backup without leaving a half-restored directory.
    let Some(checkpoint) = read_checkpoint(backup_prefix)? else {
        return Err(EhError::not_found(format!(
            "no checkpoint in {}",
            backup_prefix.display()
        )));
    };
    if data_dir.exists() {
        let mut entries = fs::read_dir(data_dir)?;
        if entries.next().is_some() {
            return Err(EhError::conflict(format!(
                "restore target {} is not empty",
                data_dir.display()
            )));
        }
    }
    fs::create_dir_all(data_dir)?;
    copy_tree(backup_prefix, data_dir, &["checkpoint.json"])?;
    tracing::info!(
        instance_id = %checkpoint.instance_id,
        domain = %checkpoint.domain,
        "backup restored into empty data dir"
    );
    Ok(checkpoint)
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

fn copy_if_absent(src: &Path, dest: &Path) -> Result<(), EhError> {
    if dest.exists() {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(src, dest)?;
    if let Some(parent) = dest.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
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
    Ok(())
}

fn copy_tree(src: &Path, dest: &Path, skip: &[&str]) -> Result<(), EhError> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if skip.iter().any(|item| *item == name_str) {
            continue;
        }
        let from = entry.path();
        let to = dest.join(&name);
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&to)?;
            copy_tree(&from, &to, &[])?;
        } else if !to.exists() {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
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
