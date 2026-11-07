//! The object-store seam a backup ships through.
//!
//! A backup prefix holds three kinds of object, with the same rules on every
//! target:
//!
//! | Object | Write | Why |
//! |---|---|---|
//! | sealed parts, snapshots, `instance.json`, schema files | [`ObjectStore::put_if_absent`] | immutable: a name is never rewritten, so a retried or concurrent ship cannot change a shipped object |
//! | `namespaces/<ns>/manifest.json`, `namespaces/<ns>/head.json` | [`ObjectStore::put`] | replaced whole; the head is written after the objects it names |
//! | `checkpoint.json` | [`ObjectStore::put`] | written last of all, so it never names a version whose objects are missing |
//!
//! Keys are `/`-separated paths relative to the prefix. A `put` must replace the
//! object atomically: a reader sees the old bytes or the new bytes, never a mix.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use ehgraph_core::EhError;

/// Where a backup lives. Implementations: [`FsStore`] (a directory) and
/// [`crate::gcs::GcsStore`] (a Google Cloud Storage bucket prefix).
pub trait ObjectStore: Send + Sync + fmt::Debug {
    /// A human-readable location for logs and errors, e.g. `gs://bucket/prefix`.
    fn describe(&self) -> String;

    /// Write `bytes` at `key` unless an object is already there. Returns `true` when
    /// this call wrote it, `false` when it already existed (the existing object is
    /// left alone). Retrying after an ambiguous failure is safe.
    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<bool, EhError>;

    /// [`ObjectStore::put_if_absent`] from a local file. The default reads the file
    /// into memory; [`FsStore`] copies it.
    fn put_file_if_absent(&self, key: &str, src: &Path) -> Result<bool, EhError> {
        if self.exists(key)? {
            return Ok(false);
        }
        self.put_if_absent(key, &fs::read(src)?)
    }

    /// Replace the object at `key` atomically.
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), EhError>;

    /// The object's bytes, or `None` when there is no object at `key`.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, EhError>;

    /// Whether an object exists at `key`.
    fn exists(&self, key: &str) -> Result<bool, EhError>;

    /// Every key under the prefix, relative to it, sorted.
    fn list(&self) -> Result<Vec<String>, EhError>;
}

/// The key for a data-dir-relative path: always `/`-separated.
pub fn key_for(relative: &Path) -> String {
    relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Refuse a key that could escape the prefix or the restore directory.
pub(crate) fn check_key(key: &str) -> Result<(), EhError> {
    let bad = key.is_empty()
        || key.starts_with('/')
        || key.contains('\\')
        || key
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..");
    if bad {
        return Err(EhError::invalid(format!(
            "backup object key {key:?} is not a relative path"
        )));
    }
    Ok(())
}

/// A backup prefix that is a local (or mounted) directory. This is the original
/// target: `put_if_absent` copies only when absent and fsyncs the directory, and
/// `put` writes a temporary file and renames it.
#[derive(Debug, Clone)]
pub struct FsStore {
    root: PathBuf,
}

impl FsStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, key: &str) -> Result<PathBuf, EhError> {
        check_key(key)?;
        Ok(self.root.join(key))
    }
}

impl ObjectStore for FsStore {
    fn describe(&self) -> String {
        self.root.display().to_string()
    }

    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<bool, EhError> {
        let dest = self.path(key)?;
        if dest.exists() {
            return Ok(false);
        }
        atomic_write(&dest, bytes)?;
        Ok(true)
    }

    fn put_file_if_absent(&self, key: &str, src: &Path) -> Result<bool, EhError> {
        let dest = self.path(key)?;
        if dest.exists() {
            return Ok(false);
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, &dest)?;
        fs::File::open(&dest)?.sync_all()?;
        if let Some(parent) = dest.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(true)
    }

    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), EhError> {
        atomic_write(&self.path(key)?, bytes)
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, EhError> {
        let path = self.path(key)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    fn exists(&self, key: &str) -> Result<bool, EhError> {
        Ok(self.path(key)?.exists())
    }

    fn list(&self) -> Result<Vec<String>, EhError> {
        let mut out = Vec::new();
        if self.root.exists() {
            walk(&self.root, &self.root, &mut out)?;
        }
        out.sort();
        Ok(out)
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), EhError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk(root, &path, out)?;
        } else if path.extension().is_none_or(|ext| ext != "tmp") {
            out.push(key_for(path.strip_prefix(root).unwrap_or(&path)));
        }
    }
    Ok(())
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), EhError> {
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
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_relative_and_slash_separated() {
        assert_eq!(
            key_for(Path::new("namespaces/lab/head.json")),
            "namespaces/lab/head.json"
        );
        for bad in ["", "/abs", "a/../b", "a//b", "./a", "a\\b"] {
            assert!(check_key(bad).is_err(), "{bad:?}");
        }
        assert!(check_key("namespaces/lab/parts/0001.part").is_ok());
    }

    #[test]
    fn fs_put_if_absent_never_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::new(dir.path());
        assert!(store.put_if_absent("a/b.part", b"one").unwrap());
        assert!(!store.put_if_absent("a/b.part", b"two").unwrap());
        assert_eq!(store.get("a/b.part").unwrap().unwrap(), b"one");
        store.put("a/head.json", b"1").unwrap();
        store.put("a/head.json", b"2").unwrap();
        assert_eq!(store.get("a/head.json").unwrap().unwrap(), b"2");
        assert_eq!(store.get("missing").unwrap(), None);
        assert_eq!(store.list().unwrap(), vec!["a/b.part", "a/head.json"]);
    }
}
