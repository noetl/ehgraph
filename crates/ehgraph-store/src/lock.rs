use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use ehgraph_core::EhError;

/// Data directories this process has open, by canonical path.
///
/// ⚠ This is the in-process half of the single-writer guarantee, and the half that
/// matters once ehgraph is embedded in a host process (noetl-server). Two handles in
/// one process on one directory are two writers with two in-memory heads.
///
/// `flock` does refuse a second open file description in the same process on Linux
/// and macOS (measured on macOS, 2026-11-06). But that depends on the platform and
/// on the filesystem: some network mounts emulate `flock` or ignore it. The
/// in-process claim does not depend on either.
fn claims() -> &'static Mutex<HashSet<PathBuf>> {
    static CLAIMS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    CLAIMS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Exclusive hold on `data_dir`: a process-wide claim on its canonical path, then a
/// non-blocking `flock` on `data_dir/LOCK`.
///
/// The claim refuses a second open in this process. The `flock` refuses a second
/// process. Both are released on drop.
pub struct DirLock {
    file: File,
    claimed: PathBuf,
}

impl DirLock {
    pub fn acquire(data_dir: &Path) -> Result<Self, EhError> {
        std::fs::create_dir_all(data_dir)?;
        let claimed = data_dir.canonicalize()?;
        {
            let mut held = claims().lock().unwrap_or_else(|p| p.into_inner());
            if !held.insert(claimed.clone()) {
                return Err(EhError::conflict(format!(
                    "data directory {} is locked: already open in this process; this instance is a single writer",
                    claimed.display()
                )));
            }
        }
        match Self::flock(&claimed) {
            Ok(file) => Ok(Self { file, claimed }),
            Err(err) => {
                release_claim(&claimed);
                Err(err)
            }
        }
    }

    fn flock(dir: &Path) -> Result<File, EhError> {
        let path = dir.join("LOCK");
        // std opens with O_CLOEXEC, so a host that spawns subprocesses does not leak
        // the lock into them.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        // SAFETY: `file` is an open fd. LOCK_EX | LOCK_NB cannot block and does not
        // invalidate the descriptor. The lock is released when `file` is closed.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Err(EhError::conflict(format!(
                "data directory {} is locked by another ehgraph process ({err}); this instance is a single writer",
                dir.display()
            )));
        }
        Ok(file)
    }

    /// Whether this process holds `data_dir` open.
    pub fn held_in_process(data_dir: &Path) -> bool {
        let Ok(path) = data_dir.canonicalize() else {
            return false;
        };
        claims()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&path)
    }
}

fn release_claim(path: &Path) {
    claims()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(path);
}

impl Drop for DirLock {
    fn drop(&mut self) {
        // SAFETY: the same fd we locked. Best-effort unlock before close.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
        release_claim(&self.claimed);
    }
}
