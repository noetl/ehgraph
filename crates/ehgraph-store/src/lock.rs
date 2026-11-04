use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;

use ehgraph_core::EhError;

/// Exclusive non-blocking lock on `data_dir/LOCK`.
///
/// Linux `flock` is per process. A second process fails. The same process can lock
/// the file again, so the single-writer test spawns a process.
pub struct DirLock {
    file: File,
}

impl DirLock {
    pub fn acquire(data_dir: &Path) -> Result<Self, EhError> {
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join("LOCK");
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
                data_dir.display()
            )));
        }
        Ok(Self { file })
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        // SAFETY: the same fd we locked. Best-effort unlock before close.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
