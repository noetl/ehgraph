//! **ehgraph** — the topology graph engine, embeddable or standalone.
//!
//! One [`EhGraph`] is one single-primary instance: a durable per-namespace change
//! log, Arrow snapshots, session-pinned in-memory graphs, and the MCP/HTTP tool
//! surface with per-namespace bearer grants. The same handle runs in two modes:
//!
//! - **Embedded** ([`EhGraph::open_embedded`]): in-process inside a host such as
//!   noetl-server. The host calls tools directly ([`EhGraph::call`],
//!   [`Client`]) with no network hop, and may also mount [`EhGraph::router`] on its
//!   own listener so `kind: mcp` playbooks reach the same instance over HTTP.
//! - **Standalone** ([`EhGraph::open_standalone`]): the `ehgraph` server binary
//!   (`ehgraph-server`) binds the same router to its own listener, one instance per
//!   StatefulSet.
//!
//! # What embedding changes
//!
//! - **Single writer (E2).** Three guards, from cheapest to widest:
//!   1. the data-directory lock: a second handle on the same directory, in this
//!      process or another on the same volume, is refused;
//!   2. a process-wide identity claim: a second embedded primary for the same
//!      `instance_id` *or* `domain` in this process is refused even on another
//!      directory;
//!   3. the host's declaration ([`EmbeddedConfig`]): an embedded instance refuses
//!      to start unless the host declares `host_replicas = 1` (and, where it has
//!      them, ordinal 0 / leader). Two host pods on two volumes can't see each
//!      other's locks, so embedded mode never relies on the lock across pods.
//!
//!   A domain whose host can't make that declaration runs standalone.
//! - **Lifecycle.** The host owns the tokio runtime, tracing, and signals. This
//!   crate installs none of them. Background maintenance (session sweep, scheduled
//!   backup) runs only after [`EhGraph::start_maintenance`], is tied to the handle,
//!   and stops on [`EhGraph::shutdown`] or drop. `shutdown` also releases the
//!   data-directory lock, so the host can hand the directory over (restore,
//!   restart) without exiting.
//! - **Blocking I/O.** Writes fsync. Use [`EhGraph::call_async`] /
//!   [`EhGraph::open_embedded_async`] from async code; the HTTP routes already do.
//! - **Auth.** In-process calls present a bearer token and are checked against
//!   the same per-namespace grants as HTTP. `auth.mode = "disabled"` is refused for
//!   an embedded instance unless `auth.allow_disabled_embedded = true`, because its
//!   routes are mounted on the host's listener, not a loopback-only port.

mod auth;
pub mod budgets;
mod config;
mod http;
mod metrics;

use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use ehgraph_api::Engine;
use ehgraph_memory::SessionRegistry;
use ehgraph_store::Store;
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub use auth::{authenticate_header, authenticate_token};
pub use config::{
    AuthConfig, BudgetConfig, Budgets, EmbeddedConfig, Grant, GraphConfig, SessionConfig,
    StoreSection,
};
pub use ehgraph_api::{Caller, ToolSpec};
pub use ehgraph_backup::{
    restore, Backup, Checkpoint, ObjectStore, TargetConfig as BackupTargetConfig, TokenProvider,
};

/// Host-supplied pieces for [`EhGraph::open_embedded_with`].
#[derive(Clone, Default)]
pub struct OpenOptions {
    /// Credentials for a `gs://` backup target, overriding `[backup] auth`. A host
    /// that already holds a Google token source (noetl-server's `gcp_auth`) passes it
    /// here so the embedded graph does not run a second auth path. Without it the
    /// default is the GKE metadata server, which on GKE is the same Workload Identity.
    pub backup_token: Option<Arc<dyn TokenProvider>>,
}

impl std::fmt::Debug for OpenOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenOptions")
            .field("backup_token", &self.backup_token.is_some())
            .finish()
    }
}
pub use ehgraph_core::{EhError, Envelope, ErrorCode, InstanceIdentity, Role};

/// How an instance is reached. Reported by `/healthz` and the `ehgraph_info` metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// In-process inside a host (noetl-server).
    Embedded,
    /// The `ehgraph` server binary.
    Standalone,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Standalone => "standalone",
        }
    }
}

/// What [`EhGraph::shutdown`] achieved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    /// The store was dropped and the data-directory lock released before the
    /// timeout. `false` means calls that started before shutdown still hold the
    /// engine; the lock is released when the last one returns.
    pub lock_released: bool,
    /// Maintenance tasks that were stopped.
    pub tasks_stopped: usize,
}

/// One ehgraph instance. Cheap to clone; all clones are the same instance.
#[derive(Clone)]
pub struct EhGraph {
    shared: Arc<Shared>,
}

struct Shared {
    config: GraphConfig,
    mode: Mode,
    identity: InstanceIdentity,
    build: String,
    /// `None` after shutdown. Taken out under the write lock so no new call can
    /// start once shutdown begins.
    engine: RwLock<Option<Arc<Engine>>>,
    maintenance: Mutex<Option<Maintenance>>,
}

struct Maintenance {
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // A handle dropped without `shutdown` must not leave loops running on the
        // host's runtime. They hold only a `Weak`, so they would exit on their next
        // tick anyway; aborting makes it immediate.
        if let Some(m) = self.maintenance.get_mut().ok().and_then(Option::take) {
            let _ = m.stop.send(true);
            for task in m.tasks {
                task.abort();
            }
        }
    }
}

impl EhGraph {
    /// Open an instance in-process, inside a host. Blocking: it takes the
    /// data-directory lock and replays the change log. From async code use
    /// [`EhGraph::open_embedded_async`].
    pub fn open_embedded(config: GraphConfig) -> Result<Self, EhError> {
        Self::open(config, Mode::Embedded, OpenOptions::default())
    }

    /// [`EhGraph::open_embedded`] with host-supplied [`OpenOptions`].
    pub fn open_embedded_with(config: GraphConfig, options: OpenOptions) -> Result<Self, EhError> {
        Self::open(config, Mode::Embedded, options)
    }

    /// [`EhGraph::open_embedded`] on the blocking pool.
    pub async fn open_embedded_async(config: GraphConfig) -> Result<Self, EhError> {
        Self::open_embedded_with_async(config, OpenOptions::default()).await
    }

    /// [`EhGraph::open_embedded_with`] on the blocking pool.
    pub async fn open_embedded_with_async(
        config: GraphConfig,
        options: OpenOptions,
    ) -> Result<Self, EhError> {
        tokio::task::spawn_blocking(move || Self::open(config, Mode::Embedded, options))
            .await
            .map_err(|err| EhError::unavailable(format!("open task failed: {err}")))?
    }

    /// Open an instance for the standalone server binary.
    pub fn open_standalone(config: GraphConfig) -> Result<Self, EhError> {
        Self::open(config, Mode::Standalone, OpenOptions::default())
    }

    fn open(config: GraphConfig, mode: Mode, options: OpenOptions) -> Result<Self, EhError> {
        config.validate()?;
        ehgraph_api::ensure_primary(config.role()?)?;
        if mode == Mode::Embedded
            && config.auth.mode == "disabled"
            && !config.auth.allow_disabled_embedded
        {
            return Err(EhError::invalid(
                "auth.mode = \"disabled\" is refused for an embedded instance: its routes are \
                 mounted on the host's listener. Use bearer grants, or set \
                 auth.allow_disabled_embedded = true for a local-only host",
            ));
        }
        if mode == Mode::Embedded {
            config.embedded.check_single_writer(&config.domain)?;
        }
        let identity = config.identity()?;
        let build = config.build_label();
        let budgets = config.resolved_budgets(mode == Mode::Embedded)?;
        let mut store_config = config.store_config();
        store_config.claim_identity = mode == Mode::Embedded;
        store_config.resident_graph_bytes = budgets.resident_graph_bytes;
        let backup = Arc::new(Backup::new(config.backup_target(options.backup_token)?));
        let store = Arc::new(Store::open(store_config)?);
        // Best effort: know the last checkpoint before the first metrics scrape, so a
        // restart does not report the whole log as unshipped. A failure here (bucket
        // unreachable) must not stop the graph from opening; status retries it.
        if let Err(err) = backup.checkpoint() {
            tracing::warn!(%err, target = %backup.describe(), "could not read the backup checkpoint at open");
        }
        let sessions = Arc::new(SessionRegistry::new(config.limits(&budgets)));
        let engine = Arc::new(Engine {
            store,
            sessions,
            identity: identity.clone(),
            backup,
            build: build.clone(),
            global_bytes: budgets.global_bytes,
            per_session_bytes: budgets.per_session_bytes,
            resident_graph_bytes: budgets.resident_graph_bytes,
        });
        tracing::info!(
            instance_id = %identity.instance_id,
            domain = %identity.domain,
            mode = mode.as_str(),
            resident_graph_bytes = budgets.resident_graph_bytes,
            session_global_bytes = budgets.global_bytes,
            host_memory_bytes = budgets.host_memory_bytes.unwrap_or(0),
            "ehgraph instance opened"
        );
        Ok(Self {
            shared: Arc::new(Shared {
                config,
                mode,
                identity,
                build,
                engine: RwLock::new(Some(engine)),
                maintenance: Mutex::new(None),
            }),
        })
    }

    pub fn identity(&self) -> &InstanceIdentity {
        &self.shared.identity
    }

    pub fn mode(&self) -> Mode {
        self.shared.mode
    }

    pub fn config(&self) -> &GraphConfig {
        &self.shared.config
    }

    pub fn build(&self) -> &str {
        &self.shared.build
    }

    /// `false` once [`EhGraph::shutdown`] has begun.
    pub fn is_serving(&self) -> bool {
        self.shared
            .engine
            .read()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
    }

    fn engine(&self) -> Result<Arc<Engine>, EhError> {
        self.shared
            .engine
            .read()
            .map_err(|_| EhError::unavailable("engine lock poisoned"))?
            .clone()
            .ok_or_else(|| {
                EhError::unavailable(format!(
                    "ehgraph instance {} is shut down",
                    self.shared.identity.instance_id
                ))
            })
    }

    /// The tools this instance serves (the `tools/list` set).
    pub fn tools(&self) -> Vec<ToolSpec> {
        ehgraph_api::tool_specs()
    }

    /// Resolve a bearer token to its grants.
    pub fn authorize(&self, token: &str) -> Result<Caller, EhError> {
        authenticate_token(&self.shared.config.auth, token)
    }

    /// Resolve an `Authorization` header value to its grants.
    pub fn authorize_header(&self, header: Option<&str>) -> Result<Caller, EhError> {
        authenticate_header(&self.shared.config.auth, header)
    }

    /// Call one tool in-process with a bearer token. Blocking.
    ///
    /// Returns the same envelope the HTTP and MCP routes return. Auth failures and
    /// a shut-down instance are failure envelopes, not panics.
    pub fn call(&self, token: &str, tool: &str, args: &Value) -> Envelope {
        match self.authorize(token) {
            Ok(caller) => self.call_as(&caller, tool, args),
            Err(err) => Envelope::failure(&self.shared.identity, &err),
        }
    }

    pub(crate) fn call_with_header(
        &self,
        header: Option<&str>,
        tool: &str,
        args: &Value,
    ) -> Envelope {
        match self.authorize_header(header) {
            Ok(caller) => self.call_as(&caller, tool, args),
            Err(err) => Envelope::failure(&self.shared.identity, &err),
        }
    }

    fn call_as(&self, caller: &Caller, tool: &str, args: &Value) -> Envelope {
        match self.engine() {
            Ok(engine) => engine.call(tool, args, caller, ehgraph_core::now_ms()),
            Err(err) => Envelope::failure(&self.shared.identity, &err),
        }
    }

    /// [`EhGraph::call`] on the blocking pool, for async hosts.
    pub async fn call_async(&self, token: String, tool: String, args: Value) -> Envelope {
        let this = self.clone();
        let identity = self.shared.identity.clone();
        tokio::task::spawn_blocking(move || this.call(&token, &tool, &args))
            .await
            .unwrap_or_else(|err| {
                Envelope::failure(
                    &identity,
                    &EhError::unavailable(format!("engine task failed: {err}")),
                )
            })
    }

    /// A client bound to one token's grants, resolved once.
    pub fn client(&self, token: &str) -> Result<Client, EhError> {
        Ok(Client {
            graph: self.clone(),
            caller: self.authorize(token)?,
        })
    }

    /// The MCP + HTTP routes for this instance. The standalone server serves them
    /// at the root. An embedded host nests them under its own prefix.
    pub fn router(&self) -> axum::Router {
        http::router(self.clone())
    }

    /// Prometheus text for this instance.
    pub fn metrics_text(&self) -> String {
        match self.engine() {
            Ok(engine) => metrics::render(&engine, self.shared.mode.as_str()),
            Err(err) => format!("# {err}\n"),
        }
    }

    /// Seal and ship the change log to `backup_prefix` (a directory or `gs://`) now. Host-trusted: no
    /// grant check. The HTTP admin route requires an operator grant before it
    /// calls this.
    pub fn backup_now(&self) -> Result<Checkpoint, EhError> {
        let engine = self.engine()?;
        let store_cfg = &self.shared.config.store;
        let (checkpoint, ()) =
            engine
                .backup
                .ship_then(&engine.store, ehgraph_core::now_ms(), |checkpoint| {
                    if store_cfg.compaction {
                        compact_shipped(&engine.store, checkpoint, store_cfg.compact_min_parts);
                    }
                })?;
        Ok(checkpoint)
    }

    /// Expire idle and over-lifetime sessions now.
    pub fn sweep_now(&self) -> Result<(), EhError> {
        self.engine()?.sessions.sweep(ehgraph_core::now_ms())
    }

    /// Start the session sweep (every second) and, when `backup_interval_s > 0`,
    /// the scheduled backup, on the current tokio runtime.
    ///
    /// Without this, idle sessions are never expired and the budget fills — the
    /// standalone binary always starts it. Errors outside a runtime, after
    /// shutdown, or when already started.
    pub fn start_maintenance(&self) -> Result<(), EhError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            EhError::invalid("start_maintenance must be called inside a tokio runtime")
        })?;
        self.engine()?;
        let mut guard = self
            .shared
            .maintenance
            .lock()
            .map_err(|_| EhError::unavailable("maintenance lock poisoned"))?;
        if guard.is_some() {
            return Err(EhError::conflict("maintenance is already running"));
        }
        let (stop, stop_rx) = watch::channel(false);
        let mut tasks = Vec::new();
        let weak = Arc::downgrade(&self.shared);
        tasks.push(runtime.spawn(every(
            Duration::from_secs(1),
            stop_rx.clone(),
            weak.clone(),
            |graph| {
                let _ = graph.sweep_now();
            },
        )));
        let interval = self.shared.config.backup_interval_s;
        if interval > 0 {
            tasks.push(runtime.spawn(every(
                Duration::from_secs(interval),
                stop_rx,
                weak,
                |graph| {
                    if let Err(err) = graph.backup_now() {
                        tracing::error!(
                            %err,
                            instance_id = %graph.identity().instance_id,
                            "scheduled backup failed"
                        );
                    }
                },
            )));
        }
        *guard = Some(Maintenance { stop, tasks });
        Ok(())
    }

    /// Stop maintenance, refuse new calls, and release the store (and so the
    /// data-directory lock) once in-flight calls finish, waiting up to `timeout`.
    /// Idempotent.
    pub async fn shutdown(&self, timeout: Duration) -> ShutdownReport {
        let maintenance = self
            .shared
            .maintenance
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        let mut tasks_stopped = 0;
        if let Some(m) = maintenance {
            let _ = m.stop.send(true);
            for task in m.tasks {
                let _ = task.await;
                tasks_stopped += 1;
            }
        }
        let engine = self
            .shared
            .engine
            .write()
            .ok()
            .and_then(|mut guard| guard.take());
        let Some(engine) = engine else {
            return ShutdownReport {
                lock_released: true,
                tasks_stopped,
            };
        };
        let deadline = Instant::now() + timeout;
        while Arc::strong_count(&engine) > 1 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let lock_released = Arc::strong_count(&engine) == 1;
        let data_dir = engine.store.data_dir().to_path_buf();
        // Dropping the store fsyncs nothing new: every acknowledged write is
        // already durable (Principle II). It closes the lock file.
        tokio::task::spawn_blocking(move || drop(engine)).await.ok();
        tracing::info!(
            instance_id = %self.shared.identity.instance_id,
            data_dir = %data_dir.display(),
            lock_released,
            "ehgraph instance shut down"
        );
        ShutdownReport {
            lock_released,
            tasks_stopped,
        }
    }
}

/// Compact every namespace up to what `checkpoint` shipped (C02). Failures are
/// logged: the backup itself succeeded, and compaction retries on the next one.
fn compact_shipped(store: &Store, checkpoint: &Checkpoint, min_parts: usize) {
    for (namespace, shipped) in &checkpoint.namespaces {
        match store.compact(namespace, shipped.backed_up_version, min_parts) {
            Ok(report) if report.dropped_parts > 0 => tracing::info!(
                namespace,
                dropped_parts = report.dropped_parts,
                floor_version = report.floor_version,
                "compaction after backup"
            ),
            Ok(_) => {}
            Err(err) => tracing::error!(namespace, %err, "compaction after backup failed"),
        }
    }
}

async fn every(
    period: Duration,
    mut stop: watch::Receiver<bool>,
    weak: Weak<Shared>,
    tick: impl Fn(&EhGraph) + Send + Sync + Clone + 'static,
) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(period) => {}
            _ = stop.changed() => return,
        }
        let Some(shared) = weak.upgrade() else {
            return;
        };
        let graph = EhGraph { shared };
        let tick = tick.clone();
        // The tick does blocking I/O; keep it off the host's async workers.
        let _ = tokio::task::spawn_blocking(move || tick(&graph)).await;
    }
}

/// A token's grants bound to one instance. Cheap to clone.
#[derive(Clone)]
pub struct Client {
    graph: EhGraph,
    caller: Caller,
}

impl Client {
    pub fn principal(&self) -> &str {
        &self.caller.principal
    }

    /// Blocking tool call.
    pub fn call(&self, tool: &str, args: &Value) -> Envelope {
        self.graph.call_as(&self.caller, tool, args)
    }

    /// Tool call on the blocking pool.
    pub async fn call_async(&self, tool: &str, args: Value) -> Envelope {
        let this = self.clone();
        let tool = tool.to_owned();
        let identity = self.graph.shared.identity.clone();
        tokio::task::spawn_blocking(move || this.call(&tool, &args))
            .await
            .unwrap_or_else(|err| {
                Envelope::failure(
                    &identity,
                    &EhError::unavailable(format!("engine task failed: {err}")),
                )
            })
    }
}
