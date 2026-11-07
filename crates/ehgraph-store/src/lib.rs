//! Durable storage for one ehgraph instance.
//!
//! The change log is the authority. Snapshots and the in-memory materialized graph are
//! derived and can be rebuilt. This crate does not link EHDB. The on-disk framing
//! follows the same shape EHDB L0 uses (magic, length, crc32, body; immutable sealed
//! parts; a versioned manifest; a head pointer written after the payload) so a later
//! replica can tail `graph_version` without adopting EHDB's dataset model.
//!
//! Graph bytes never go to an EHDB instance or dataset. They live under `data_dir`.

mod arrow_batch;
pub mod format;
mod frame;
mod lock;
pub use lock::{DirLock, IdentityClaim};
mod state;
mod store;

pub use arrow_batch::{batch_memory_size, edges_to_batch, nodes_to_batch};
pub use state::{KeyLimits, KeyWindow, MaterializedGraph};
pub use store::{
    BackupNamespace, BackupPlan, CompactCrash, CompactionReport, DeactivateRequest, NsSummary,
    SnapshotInfo, Store, StoreConfig, VersionInfo, WriteOutcome,
};
