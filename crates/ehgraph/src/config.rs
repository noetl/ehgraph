use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ehgraph_core::{EhError, InstanceIdentity, Role};
use ehgraph_memory::Limits;
use ehgraph_store::StoreConfig;
use serde::Deserialize;

/// One instance's configuration, independent of how it is reached.
///
/// The same struct configures an embedded instance (inside a host such as
/// noetl-server) and the standalone server, which adds only `listen`.
#[derive(Debug, Clone, Deserialize)]
pub struct GraphConfig {
    pub instance_id: String,
    pub domain: String,
    pub data_dir: PathBuf,
    pub backup_prefix: PathBuf,
    #[serde(default = "default_role")]
    pub role: String,
    #[serde(default)]
    pub build: Option<String>,
    #[serde(default)]
    pub budgets: BudgetConfig,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub store: StoreSection,
    #[serde(default)]
    pub auth: AuthConfig,
    /// When non-zero, the running server ships sealed parts on this interval.
    #[serde(default)]
    pub backup_interval_s: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BudgetConfig {
    #[serde(default = "default_global")]
    pub global_bytes: u64,
    #[serde(default = "default_session_cap")]
    pub per_session_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionConfig {
    #[serde(default = "default_idle")]
    pub idle_timeout_s: u64,
    #[serde(default = "default_lifetime")]
    pub max_lifetime_s: u64,
    #[serde(default = "default_grace")]
    pub grace_s: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StoreSection {
    #[serde(default = "default_seal_bytes")]
    pub seal_max_bytes: u64,
    #[serde(default = "default_seal_records")]
    pub seal_max_records: u64,
    #[serde(default = "default_seal_age")]
    pub seal_max_age_s: u64,
    #[serde(default = "default_keep")]
    pub snapshot_keep: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    /// `bearer` or `disabled` (local laptops and tests).
    #[serde(default = "default_auth_mode")]
    pub mode: String,
    /// ⚠ Embedded only. An embedded instance's routes are mounted on the HOST's
    /// listener, which is usually not loopback-only, so `mode = "disabled"` is refused
    /// there unless this is set too. Ignored by the standalone server.
    #[serde(default)]
    pub allow_disabled_embedded: bool,
    #[serde(default)]
    pub grants: Vec<Grant>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Grant {
    pub token: String,
    pub principal: String,
    pub namespace: String,
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub write: bool,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            global_bytes: default_global(),
            per_session_bytes: default_session_cap(),
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            idle_timeout_s: default_idle(),
            max_lifetime_s: default_lifetime(),
            grace_s: default_grace(),
        }
    }
}

impl Default for StoreSection {
    fn default() -> Self {
        Self {
            seal_max_bytes: default_seal_bytes(),
            seal_max_records: default_seal_records(),
            seal_max_age_s: default_seal_age(),
            snapshot_keep: default_keep(),
        }
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            mode: default_auth_mode(),
            allow_disabled_embedded: false,
            grants: Vec::new(),
        }
    }
}

impl GraphConfig {
    pub fn load(path: &Path) -> Result<Self, EhError> {
        let text = fs::read_to_string(path)?;
        Self::from_toml(&text).map_err(|err| {
            EhError::new(
                err.code,
                format!("config {}: {}", path.display(), err.message),
            )
        })
    }

    /// Parse and validate a TOML document. A host that keeps ehgraph's settings in
    /// its own config file can hand over the sub-table's text.
    pub fn from_toml(text: &str) -> Result<Self, EhError> {
        let cfg: GraphConfig = toml::from_str(text)
            .map_err(|err| EhError::invalid(format!("ehgraph config: {err}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), EhError> {
        safe_label("instance_id", &self.instance_id)?;
        safe_label("domain", &self.domain)?;
        if self.role != "primary" && self.role != "replica" && self.role != "standby" {
            return Err(EhError::invalid(
                "role must be primary, replica, or standby",
            ));
        }
        if self.auth.mode != "bearer" && self.auth.mode != "disabled" {
            return Err(EhError::invalid("auth.mode must be bearer or disabled"));
        }
        Ok(())
    }

    pub fn role(&self) -> Result<Role, EhError> {
        match self.role.as_str() {
            "primary" => Ok(Role::Primary),
            "replica" => Ok(Role::Replica),
            "standby" => Ok(Role::Standby),
            _ => Err(EhError::invalid("unknown role")),
        }
    }

    pub fn identity(&self) -> Result<InstanceIdentity, EhError> {
        Ok(InstanceIdentity {
            instance_id: self.instance_id.clone(),
            domain: self.domain.clone(),
            role: self.role()?,
        })
    }

    pub fn store_config(&self) -> StoreConfig {
        StoreConfig {
            instance_id: self.instance_id.clone(),
            domain: self.domain.clone(),
            data_dir: self.data_dir.clone(),
            seal_max_bytes: self.store.seal_max_bytes,
            seal_max_records: self.store.seal_max_records,
            seal_max_age: Duration::from_secs(self.store.seal_max_age_s),
            snapshot_keep: self.store.snapshot_keep,
        }
    }

    pub fn limits(&self) -> Limits {
        Limits {
            global_bytes: self.budgets.global_bytes,
            per_session_bytes: self.budgets.per_session_bytes,
            default_idle: Duration::from_secs(self.session.idle_timeout_s),
            default_max_lifetime: Duration::from_secs(self.session.max_lifetime_s),
            grace: Duration::from_secs(self.session.grace_s),
        }
    }

    pub fn build_label(&self) -> String {
        self.build
            .clone()
            .unwrap_or_else(|| format!("ehgraph {}", env!("CARGO_PKG_VERSION")))
    }
}

fn safe_label(field: &str, value: &str) -> Result<(), EhError> {
    if value.is_empty()
        || value
            .chars()
            .any(|c| c.is_control() || c == '"' || c == '\\' || c == '{' || c == '}')
    {
        return Err(EhError::invalid(format!(
            "{field} must be a non-empty label without quotes or control characters"
        )));
    }
    Ok(())
}

fn default_role() -> String {
    "primary".into()
}
fn default_global() -> u64 {
    256 * 1024 * 1024
}
fn default_session_cap() -> u64 {
    64 * 1024 * 1024
}
fn default_idle() -> u64 {
    300
}
fn default_lifetime() -> u64 {
    24 * 60 * 60
}
fn default_grace() -> u64 {
    2
}
fn default_seal_bytes() -> u64 {
    8 * 1024 * 1024
}
fn default_seal_records() -> u64 {
    1024
}
fn default_seal_age() -> u64 {
    30
}
fn default_keep() -> usize {
    16
}
fn default_auth_mode() -> String {
    "bearer".into()
}
