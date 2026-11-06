//! The `ehgraph` database server.
//!
//! One process is one primary instance. Library crates behind this binary are
//! internal. Consumers use MCP or HTTP.

mod auth;
mod config;
mod http;
mod metrics;

use std::sync::Arc;

use ehgraph_api::Engine;
use ehgraph_core::EhError;
use ehgraph_memory::SessionRegistry;
use ehgraph_store::Store;

pub use config::Config;
pub use http::AppState;

pub struct Running {
    pub router: axum::Router,
    pub state: AppState,
}

pub fn boot(config: Config) -> Result<Running, EhError> {
    config.validate()?;
    ehgraph_api::ensure_primary(config.role()?)?;
    let store = Arc::new(Store::open(config.store_config())?);
    let sessions = Arc::new(SessionRegistry::new(config.limits()));
    let engine = Arc::new(Engine {
        store,
        sessions,
        identity: config.identity()?,
        backup_prefix: config.backup_prefix.clone(),
        build: config.build_label(),
        global_bytes: config.budgets.global_bytes,
        per_session_bytes: config.budgets.per_session_bytes,
    });
    let state = AppState { engine, config };
    let router = http::router(state.clone());
    Ok(Running { router, state })
}
