//! Multi-statement transactions and conditional, serializable commits.
//!
//! A transaction pins a **snapshot** of one namespace (the head view at BEGIN).
//! Every statement reads that snapshot plus the transaction's own earlier writes
//! (the executor's overlay, carried in [`TxState`]). Nothing reaches the store until
//! COMMIT, so ROLLBACK, a failed statement, an expired timeout or a dropped
//! connection simply discard the transaction.
//!
//! COMMIT is optimistic concurrency control, validated under the store's write lock
//! (`Store::transact`):
//!
//! 1. `expect` preconditions (per-element `version_updated`, or the graph version);
//! 2. **read-set validation**: every element changed after the snapshot is checked
//!    against the transaction's footprint (ids it read or wrote, labels it scanned,
//!    adjacency it expanded). Any overlap is a serialization conflict;
//! 3. the net effect is appended as **one** change-log frame, so one graph version.
//!
//! Commits are serialized by the store lock and each one validates against every
//! commit after its snapshot, so the committed history is equivalent to running the
//! transactions one at a time in commit order (first committer wins). A loser gets
//! `VERSION_CONFLICT` and nothing is written; re-run the transaction.

use std::cell::RefCell;
use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ehgraph_core::{EhError, Envelope, ErrorCode, Expect};
use ehgraph_cypher::{Changes, Footprint, TxState};
use ehgraph_memory::SessionGraph;
use ehgraph_store::{MaterializedGraph, TxRequest};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::dispatch::{Caller, CypherRun, Engine};

/// Most transactions open at once, across namespaces and callers.
pub const MAX_OPEN_TXS: usize = 256;
/// Idle timeout when BEGIN names none. Each statement pushes the deadline back.
pub const DEFAULT_TX_TIMEOUT_MS: i64 = 30_000;
/// Longest idle timeout a caller may ask for.
pub const MAX_TX_TIMEOUT_MS: i64 = 600_000;

/// The open transactions of one engine.
#[derive(Default)]
pub struct TxRegistry {
    txs: Mutex<HashMap<String, Arc<Mutex<Tx>>>>,
    seq: AtomicU64,
    ids: RandomState,
}

impl TxRegistry {
    /// Number of open transactions.
    pub fn open(&self) -> usize {
        self.txs.lock().map(|txs| txs.len()).unwrap_or(0)
    }

    /// Drop transactions whose idle deadline passed. Busy ones are left alone.
    pub fn sweep(&self, now_ms: i64) {
        if let Ok(mut txs) = self.txs.lock() {
            txs.retain(|_, tx| match tx.try_lock() {
                Ok(tx) => tx.expires_ms >= now_ms,
                Err(_) => true,
            });
        }
    }

    fn new_id(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut a = self.ids.build_hasher();
        a.write_u64(n);
        a.write_i64(ehgraph_core::now_ms());
        let mut b = self.ids.build_hasher();
        b.write_u64(a.finish());
        b.write_u64(n.rotate_left(17));
        format!("tx-{:016x}{:016x}", a.finish(), b.finish())
    }
}

pub(crate) struct Tx {
    namespace: String,
    principal: String,
    base: Arc<SessionGraph>,
    state: TxState,
    failed: Option<String>,
    timeout_ms: i64,
    expires_ms: i64,
    source: String,
    execution_id: String,
}

/// Options for [`Engine::tx_begin`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TxBegin {
    pub namespace: String,
    /// Idle timeout (ms); default 30 s, at most 10 min.
    #[serde(default)]
    pub timeout_ms: Option<i64>,
    /// `source` stamped on elements the transaction writes (default `cypher`).
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub execution_id: Option<String>,
}

/// An open transaction.
#[derive(Debug, Clone)]
pub struct TxInfo {
    pub tx_id: String,
    pub namespace: String,
    /// The snapshot every statement reads.
    pub graph_version: u64,
    pub expires_ms: i64,
}

impl TxInfo {
    pub fn to_json(&self) -> Value {
        json!({
            "tx_id": self.tx_id,
            "namespace": self.namespace,
            "graph_version": self.graph_version,
            "expires_ms": self.expires_ms,
        })
    }
}

/// A committed transaction.
#[derive(Debug, Clone)]
pub struct TxCommitted {
    /// The new head when something was written; else the snapshot version.
    pub graph_version: u64,
    pub changed: bool,
    pub writes: Vec<Value>,
}

/// Why a commit did not happen.
#[derive(Debug)]
pub(crate) enum CommitError {
    /// A concurrent commit changed something the transaction read or wrote.
    Serialization(EhError),
    Other(EhError),
}

impl From<CommitError> for EhError {
    fn from(err: CommitError) -> Self {
        match err {
            CommitError::Serialization(e) | CommitError::Other(e) => e,
        }
    }
}

/// Everything one commit needs.
pub(crate) struct Commit<'a> {
    pub namespace: &'a str,
    pub base: &'a SessionGraph,
    pub changes: &'a Changes,
    pub footprint: &'a Footprint,
    pub expect: &'a Expect,
    pub idempotency_key: Option<String>,
    pub source: &'a str,
    pub execution_id: &'a str,
    pub ts_ms: i64,
}

impl Engine {
    /// The snapshot a write runs against: the head view, or an empty graph at
    /// version 0 for a namespace that does not exist yet (a write may create it).
    pub(crate) fn tx_base(&self, namespace: &str) -> Result<Arc<SessionGraph>, EhError> {
        match self.store.head(namespace) {
            Ok(_) => self.head_view(namespace),
            Err(err) if err.code == ErrorCode::NotFound => {
                ehgraph_core::validate_namespace(namespace)?;
                Ok(Arc::new(SessionGraph::from_materialized(
                    &MaterializedGraph::from_elements(0, Vec::new(), Vec::new()),
                )?))
            }
            Err(err) => Err(err),
        }
    }

    /// Validate and commit a transaction's net effect as one graph version.
    pub(crate) fn commit_tx(&self, c: Commit<'_>) -> Result<(u64, Vec<Value>), CommitError> {
        if c.changes.is_empty() && c.expect.is_empty() && c.idempotency_key.is_none() {
            // Read-only: it read one consistent snapshot, which is serializable as is.
            return Ok((c.base.version, Vec::new()));
        }
        let since = c.base.version;
        let conflict: RefCell<Option<EhError>> = RefCell::new(None);
        let validate = |head: &MaterializedGraph| -> Result<(), EhError> {
            if head.version == since {
                return Ok(());
            }
            match c
                .footprint
                .conflict(since, head.nodes.values(), head.edges.values())
            {
                None => Ok(()),
                Some(why) => {
                    let err = EhError::new(
                        ErrorCode::VersionConflict,
                        format!("serialization conflict: {why}; nothing was written, re-run the transaction"),
                    );
                    *conflict.borrow_mut() = Some(err.clone());
                    Err(err)
                }
            }
        };
        let request = TxRequest {
            namespace: c.namespace.to_string(),
            nodes: c.changes.nodes.clone(),
            edges: c.changes.edges.clone(),
            deactivate: c.changes.deactivate.clone(),
            expect: c.expect.clone(),
            idempotency_key: c.idempotency_key,
            source: c.source.to_string(),
            execution_id: c.execution_id.to_string(),
            ts_ms: c.ts_ms,
        };
        match self.store.transact(request, &validate) {
            Ok(outcome) => {
                let writes = if c.changes.is_empty() {
                    Vec::new()
                } else {
                    vec![json!({
                        "op": "commit",
                        "graph_version": outcome.graph_version,
                        "changed": outcome.changed,
                        "applied": outcome.applied,
                        "upserted": c.changes.nodes.len() + c.changes.edges.len(),
                        "deactivated": c.changes.deactivate.len(),
                    })]
                };
                Ok((outcome.graph_version, writes))
            }
            Err(err) => match conflict.into_inner() {
                Some(_) => Err(CommitError::Serialization(err)),
                None => Err(CommitError::Other(err)),
            },
        }
    }

    /// Open a transaction on `opts.namespace` as `caller`.
    pub fn tx_begin(&self, caller: &Caller, opts: TxBegin, now_ms: i64) -> Result<TxInfo, EhError> {
        self.require(caller, &opts.namespace, false)?;
        let timeout_ms = opts.timeout_ms.unwrap_or(DEFAULT_TX_TIMEOUT_MS);
        if !(1..=MAX_TX_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(EhError::invalid(format!(
                "timeout_ms must be 1..={MAX_TX_TIMEOUT_MS}"
            )));
        }
        self.txs.sweep(now_ms);
        let base = self.tx_base(&opts.namespace)?;
        let tx_id = self.txs.new_id();
        let tx = Tx {
            namespace: opts.namespace.clone(),
            principal: caller.principal.clone(),
            base: base.clone(),
            state: TxState::default(),
            failed: None,
            timeout_ms,
            expires_ms: now_ms.saturating_add(timeout_ms),
            source: opts.source.unwrap_or_else(|| "cypher".to_string()),
            execution_id: opts
                .execution_id
                .unwrap_or_else(|| format!("cypher:{tx_id}")),
        };
        let expires_ms = tx.expires_ms;
        {
            let mut txs = self
                .txs
                .txs
                .lock()
                .map_err(|_| EhError::unavailable("transaction registry poisoned"))?;
            if txs.len() >= MAX_OPEN_TXS {
                return Err(EhError::new(
                    ErrorCode::Unavailable,
                    format!(
                        "{MAX_OPEN_TXS} transactions are already open; commit or roll back first"
                    ),
                ));
            }
            txs.insert(tx_id.clone(), Arc::new(Mutex::new(tx)));
        }
        Ok(TxInfo {
            tx_id,
            namespace: opts.namespace,
            graph_version: base.version,
            expires_ms,
        })
    }

    fn tx_handle(
        &self,
        caller: &Caller,
        tx_id: &str,
        now_ms: i64,
    ) -> Result<Arc<Mutex<Tx>>, EhError> {
        self.txs.sweep(now_ms);
        let txs = self
            .txs
            .txs
            .lock()
            .map_err(|_| EhError::unavailable("transaction registry poisoned"))?;
        let handle = txs.get(tx_id).cloned().ok_or_else(|| {
            EhError::not_found(format!(
                "transaction {tx_id} not found (committed, rolled back, or expired)"
            ))
        })?;
        drop(txs);
        let principal = handle
            .lock()
            .map_err(|_| EhError::unavailable("transaction poisoned"))?
            .principal
            .clone();
        if principal != caller.principal {
            return Err(EhError::new(
                ErrorCode::Unauthorized,
                format!("transaction {tx_id} belongs to another principal"),
            ));
        }
        Ok(handle)
    }

    /// The idle deadline of an open transaction.
    pub fn tx_expires(&self, tx_id: &str) -> Option<i64> {
        let handle = self.txs.txs.lock().ok()?.get(tx_id).cloned()?;
        let expires = handle.lock().ok()?.expires_ms;
        Some(expires)
    }

    fn tx_remove(&self, tx_id: &str) {
        if let Ok(mut txs) = self.txs.txs.lock() {
            txs.remove(tx_id);
        }
    }

    /// Run one statement inside a transaction. It reads the snapshot plus the
    /// transaction's own writes; its writes stay in the transaction until COMMIT.
    pub fn tx_run(
        &self,
        caller: &Caller,
        tx_id: &str,
        query: &str,
        params: &serde_json::Map<String, Value>,
        max_rows: Option<usize>,
        now_ms: i64,
    ) -> Result<CypherRun, EhError> {
        let handle = self.tx_handle(caller, tx_id, now_ms)?;
        let mut tx = handle
            .lock()
            .map_err(|_| EhError::unavailable("transaction poisoned"))?;
        if let Some(why) = &tx.failed {
            return Err(EhError::invalid(format!(
                "transaction {tx_id} failed earlier ({why}); roll it back"
            )));
        }
        tx.expires_ms = now_ms.saturating_add(tx.timeout_ms);
        let result = (|| {
            let parsed = ehgraph_cypher::parse(query)?;
            if parsed.is_updating() {
                self.require(caller, &tx.namespace, true)?;
            }
            let params: BTreeMap<String, ehgraph_cypher::Val> = params
                .iter()
                .map(|(k, v)| (k.clone(), ehgraph_cypher::Val::from_json(v)))
                .collect();
            let opts = ehgraph_cypher::ExecOptions {
                source: tx.source.clone(),
                execution_id: tx.execution_id.clone(),
                now_ms,
                max_hops: crate::dispatch::default_hops(),
                max_rows: max_rows.unwrap_or(100_000).clamp(1, 1_000_000),
                visibility: ehgraph_core::ActiveFilter::Active,
            };
            let base = tx.base.clone();
            ehgraph_cypher::execute_in(&base, &mut tx.state, &parsed, &params, &opts)
                .map_err(EhError::from)
        })();
        match result {
            Ok(outcome) => Ok(CypherRun {
                graph_version: tx.base.version,
                outcome,
                writes: Vec::new(),
            }),
            Err(err) => {
                tx.failed = Some(err.message.clone());
                Err(err)
            }
        }
    }

    /// Commit a transaction: validate it against every commit after its snapshot,
    /// check `expect`, and write its net effect as one graph version. The
    /// transaction is closed whatever the outcome.
    pub fn tx_commit(
        &self,
        caller: &Caller,
        tx_id: &str,
        expect: &Expect,
        idempotency_key: Option<String>,
        now_ms: i64,
    ) -> Result<TxCommitted, EhError> {
        let handle = self.tx_handle(caller, tx_id, now_ms)?;
        self.tx_remove(tx_id);
        let tx = handle
            .lock()
            .map_err(|_| EhError::unavailable("transaction poisoned"))?;
        if let Some(why) = &tx.failed {
            return Err(EhError::invalid(format!(
                "transaction {tx_id} failed earlier ({why}) and was rolled back"
            )));
        }
        let changes = tx.state.changes(&tx.base);
        if !changes.is_empty() {
            self.require(caller, &tx.namespace, true)?;
        }
        let (graph_version, writes) = self.commit_tx(Commit {
            namespace: &tx.namespace,
            base: &tx.base,
            changes: &changes,
            footprint: &tx.state.footprint,
            expect,
            idempotency_key,
            source: &tx.source,
            execution_id: &tx.execution_id,
            ts_ms: now_ms,
        })?;
        Ok(TxCommitted {
            graph_version,
            changed: writes
                .first()
                .and_then(|w| w.get("changed"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            writes,
        })
    }

    /// Discard a transaction. `Ok(false)` when it was already gone.
    pub fn tx_rollback(&self, caller: &Caller, tx_id: &str, now_ms: i64) -> Result<bool, EhError> {
        match self.tx_handle(caller, tx_id, now_ms) {
            Ok(_) => {
                self.tx_remove(tx_id);
                Ok(true)
            }
            Err(err) if err.code == ErrorCode::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    // ---- tool wrappers (`graph.tx.*`) ----

    pub(crate) fn tool_tx_begin(
        &self,
        args: &Value,
        caller: &Caller,
        now_ms: i64,
    ) -> Result<Envelope, EhError> {
        let opts: TxBegin = serde_json::from_value(args.clone())
            .map_err(|err| EhError::invalid(format!("graph.tx.begin: {err}")))?;
        let info = self.tx_begin(caller, opts, now_ms)?;
        Ok(self.done(info.graph_version, info.to_json()))
    }

    pub(crate) fn tool_tx_run(
        &self,
        args: &Value,
        caller: &Caller,
        now_ms: i64,
    ) -> Result<Envelope, EhError> {
        #[derive(Deserialize)]
        struct Args {
            tx_id: String,
            query: String,
            #[serde(default)]
            params: serde_json::Map<String, Value>,
            #[serde(default)]
            max_rows: Option<usize>,
        }
        let parsed: Args = serde_json::from_value(args.clone())
            .map_err(|err| EhError::invalid(format!("graph.tx.run: {err}")))?;
        let run = self.tx_run(
            caller,
            &parsed.tx_id,
            &parsed.query,
            &parsed.params,
            parsed.max_rows,
            now_ms,
        )?;
        Ok(self.done(
            run.graph_version,
            json!({
                "tx_id": parsed.tx_id,
                "expires_ms": self.tx_expires(&parsed.tx_id),
                "columns": run.outcome.columns,
                "rows": run.outcome.rows_json(),
                "stats": run.outcome.stats.to_json(),
            }),
        ))
    }

    pub(crate) fn tool_tx_commit(
        &self,
        args: &Value,
        caller: &Caller,
        now_ms: i64,
    ) -> Result<Envelope, EhError> {
        #[derive(Deserialize)]
        struct Args {
            tx_id: String,
            #[serde(default)]
            expect: Expect,
            #[serde(default)]
            idempotency_key: Option<String>,
        }
        let parsed: Args = serde_json::from_value(args.clone())
            .map_err(|err| EhError::invalid(format!("graph.tx.commit: {err}")))?;
        let done = self.tx_commit(
            caller,
            &parsed.tx_id,
            &parsed.expect,
            parsed.idempotency_key,
            now_ms,
        )?;
        Ok(self.done(
            done.graph_version,
            json!({
                "graph_version": done.graph_version,
                "changed": done.changed,
                "writes": done.writes,
            }),
        ))
    }

    pub(crate) fn tool_tx_rollback(
        &self,
        args: &Value,
        caller: &Caller,
        now_ms: i64,
    ) -> Result<Envelope, EhError> {
        #[derive(Deserialize)]
        struct Args {
            tx_id: String,
        }
        let parsed: Args = serde_json::from_value(args.clone())
            .map_err(|err| EhError::invalid(format!("graph.tx.rollback: {err}")))?;
        let existed = self.tx_rollback(caller, &parsed.tx_id, now_ms)?;
        Ok(Envelope::success(
            &self.identity,
            None,
            json!({"tx_id": parsed.tx_id, "rolled_back": existed}),
        ))
    }
}
