//! The standalone `ehgraph` server.
//!
//! A thin wrapper: the engine, store, sessions, auth and routes all live in the
//! embeddable `ehgraph` crate. This crate adds only a listen address, the CLI,
//! tracing setup, signal handling, and process exit — the things a host process
//! owns when ehgraph is embedded instead.

use std::net::SocketAddr;
use std::path::Path;

use ehgraph::{EhError, EhGraph, GraphConfig};
use serde::Deserialize;

/// Standalone config: an instance's [`GraphConfig`] plus where to listen.
/// The TOML shape is unchanged from the MVP.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Optional Bolt listener (off unless the section is present).
    #[serde(default)]
    pub bolt: Option<BoltConfig>,
    #[serde(flatten)]
    pub graph: GraphConfig,
}

/// `[bolt]`: serve the Bolt protocol so Neo4j drivers can run Cypher.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoltConfig {
    /// e.g. "127.0.0.1:7687". Plaintext; keep it on loopback or behind TLS.
    pub listen: String,
    /// Namespace used when a client opens a session without `database=`.
    #[serde(default)]
    pub default_namespace: Option<String>,
}

impl BoltConfig {
    pub fn listen_addr(&self) -> Result<SocketAddr, EhError> {
        self.listen
            .parse()
            .map_err(|err| EhError::invalid(format!("bolt.listen {}: {err}", self.listen)))
    }
}

fn default_listen() -> String {
    "0.0.0.0:8080".into()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, EhError> {
        let text = std::fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&text)
            .map_err(|err| EhError::invalid(format!("config {}: {err}", path.display())))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), EhError> {
        self.graph.validate()?;
        self.listen_addr()?;
        if let Some(bolt) = &self.bolt {
            bolt.listen_addr()?;
        }
        Ok(())
    }

    pub fn listen_addr(&self) -> Result<SocketAddr, EhError> {
        self.listen
            .parse()
            .map_err(|err| EhError::invalid(format!("listen {}: {err}", self.listen)))
    }
}

pub struct Running {
    pub router: axum::Router,
    pub graph: EhGraph,
}

/// Open the instance in standalone mode and build its router. Does not start
/// maintenance or bind a socket; `main` does both.
pub fn boot(config: Config) -> Result<Running, EhError> {
    config.validate()?;
    let graph = EhGraph::open_standalone(config.graph)?;
    let router = graph.router();
    Ok(Running { router, graph })
}
