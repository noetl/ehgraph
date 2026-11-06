//! Session-scoped graphs resident in the ehgraph process.
//!
//! A session pins one Arrow + CSR copy until explicit close, idle expiry, or the
//! maximum lifetime. Copies are not shared across sessions. Writes never land here.

mod graph;
mod registry;

pub use graph::{
    Direction, NeighborQuery, NodeQuery, Page, PathQuery, PathResult, SessionGraph, Subgraph,
    SubgraphQuery,
};
pub use registry::{
    CloseOutcome, EventKind, Limits, OpenOutcome, OpenRequest, RefreshOutcome, SessionEvent,
    SessionInfo, SessionRegistry, SessionState,
};
