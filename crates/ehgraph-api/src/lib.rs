//! Tool operations shared by the MCP and HTTP fronts.
//!
//! Authorization is decided by the caller the server built from the request
//! credential. This crate does not read the network.

mod dispatch;

pub use dispatch::{ensure_primary, tool_specs, Caller, CypherRun, Engine, ToolSpec};
