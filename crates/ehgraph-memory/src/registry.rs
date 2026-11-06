//! Session registry: one resident graph per session, pinned until close or expiry.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ehgraph_core::{validate_namespace, validate_owner, EhError, ErrorCode, Scope};
use serde::Serialize;

use crate::graph::SessionGraph;

#[derive(Debug, Clone)]
pub struct Limits {
    pub global_bytes: u64,
    pub per_session_bytes: u64,
    pub default_idle: Duration,
    pub default_max_lifetime: Duration,
    pub grace: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            global_bytes: 256 * 1024 * 1024,
            per_session_bytes: 64 * 1024 * 1024,
            default_idle: Duration::from_secs(300),
            default_max_lifetime: Duration::from_secs(24 * 60 * 60),
            grace: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Loading,
    Active,
    Refreshing,
    Closed,
    Expired,
    Evicted,
    Failed,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Active => "active",
            Self::Refreshing => "refreshing",
            Self::Closed => "closed",
            Self::Expired => "expired",
            Self::Evicted => "evicted",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Opened,
    Loaded,
    Refreshed,
    Closed,
    Expired,
    Evicted,
    LoadFailed,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionEvent {
    pub kind: EventKind,
    pub session_id: String,
    pub owner: String,
    pub namespace: String,
    pub scope: String,
    pub version: u64,
    pub bytes: u64,
    pub ts_ms: i64,
}

#[derive(Debug, Clone)]
pub struct OpenRequest {
    pub owner: String,
    pub namespace: String,
    pub scope: Scope,
    /// `None` pins whatever version the loader returns (normally head).
    pub requested_version: Option<u64>,
    pub idle_timeout: Option<Duration>,
    pub now_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenOutcome {
    pub graph_session_id: String,
    pub snapshot_version: u64,
    pub resident_bytes: u64,
    pub load_ms: u64,
    pub reused: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefreshOutcome {
    pub snapshot_version: u64,
    pub previous_version: u64,
    pub resident_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloseOutcome {
    pub released_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub graph_session_id: String,
    pub owner: String,
    pub namespace: String,
    pub scope: String,
    pub state: SessionState,
    pub snapshot_version: u64,
    pub resident_bytes: u64,
    pub opened_at: i64,
    pub last_read_at: i64,
    pub expires_at: i64,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct LoadKey {
    owner: String,
    namespace: String,
    scope: String,
    version: String,
}

struct Session {
    id: String,
    owner: String,
    namespace: String,
    scope_key: String,
    version: u64,
    state: SessionState,
    opened_at: i64,
    last_read_at: i64,
    idle_ms: i64,
    max_lifetime_ms: i64,
    resident_bytes: u64,
    graph: Option<Arc<SessionGraph>>,
    release_at: Option<i64>,
}

struct Inner {
    sessions: HashMap<String, Session>,
    by_key: HashMap<LoadKey, String>,
    inflight: HashSet<LoadKey>,
    events: Vec<SessionEvent>,
    loads: u64,
    last_load_ms: u64,
    evictions: BTreeMap<String, u64>,
    seq: u64,
}

pub struct SessionRegistry {
    limits: Limits,
    inner: Mutex<Inner>,
    cv: Condvar,
}

impl SessionRegistry {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            inner: Mutex::new(Inner {
                sessions: HashMap::new(),
                by_key: HashMap::new(),
                inflight: HashSet::new(),
                events: Vec::new(),
                loads: 0,
                last_load_ms: 0,
                evictions: BTreeMap::new(),
                seq: 0,
            }),
            cv: Condvar::new(),
        }
    }

    pub fn open(
        &self,
        request: OpenRequest,
        loader: impl FnOnce() -> Result<SessionGraph, EhError>,
    ) -> Result<OpenOutcome, EhError> {
        validate_owner(&request.owner)?;
        validate_namespace(&request.namespace)?;
        let key = LoadKey {
            owner: request.owner.clone(),
            namespace: request.namespace.clone(),
            scope: request.scope.canonical(),
            version: request
                .requested_version
                .map(|v| v.to_string())
                .unwrap_or_else(|| "head".into()),
        };
        {
            let mut guard = self.lock()?;
            self.sweep_locked(&mut guard, request.now_ms);
            if let Some(outcome) = reuse(&guard, &key) {
                return Ok(outcome);
            }
        }
        loop {
            let mut guard = self.lock()?;
            if let Some(outcome) = reuse(&guard, &key) {
                return Ok(outcome);
            }
            if guard.inflight.contains(&key) {
                guard = self
                    .cv
                    .wait(guard)
                    .map_err(|_| EhError::unavailable("session registry poisoned"))?;
                drop(guard);
                continue;
            }
            guard.inflight.insert(key.clone());
            drop(guard);
            let started = std::time::Instant::now();
            let loaded = loader();
            let load_ms = started.elapsed().as_millis() as u64;
            let mut guard = self.lock()?;
            guard.inflight.remove(&key);
            let result = match loaded {
                Ok(graph) => self.accept(&mut guard, &request, &key, graph, load_ms),
                Err(err) => {
                    self.push_event(
                        &mut guard,
                        EventKind::LoadFailed,
                        "",
                        &request.owner,
                        &request.namespace,
                        &request.scope.canonical(),
                        0,
                        0,
                        request.now_ms,
                    );
                    Err(err)
                }
            };
            self.cv.notify_all();
            return result;
        }
    }

    pub fn refresh(
        &self,
        session_id: &str,
        now_ms: i64,
        loader: impl FnOnce(u64) -> Result<SessionGraph, EhError>,
    ) -> Result<RefreshOutcome, EhError> {
        let previous = {
            let mut guard = self.lock()?;
            self.sweep_locked(&mut guard, now_ms);
            let session = guard
                .sessions
                .get_mut(session_id)
                .ok_or_else(|| EhError::not_found(format!("session {session_id} not found")))?;
            if session.state != SessionState::Active {
                return Err(ended_error(session.state));
            }
            session.state = SessionState::Refreshing;
            session.version
        };
        let loaded = loader(previous);
        let mut guard = self.lock()?;
        let Some(session) = guard.sessions.get_mut(session_id) else {
            return Err(EhError::not_found(format!(
                "session {session_id} not found"
            )));
        };
        if session.state != SessionState::Refreshing {
            return Err(ended_error(session.state));
        }
        match loaded {
            Ok(graph) => {
                if graph.resident_bytes > self.limits.per_session_bytes {
                    session.state = SessionState::Active;
                    return Err(EhError::new(
                        ErrorCode::MemoryBudgetExceeded,
                        "refreshed scope exceeds the per-session cap",
                    ));
                }
                let bytes = graph.resident_bytes;
                let version = graph.version;
                let owner = session.owner.clone();
                let namespace = session.namespace.clone();
                let scope = session.scope_key.clone();
                session.graph = Some(Arc::new(graph));
                session.version = version;
                session.resident_bytes = bytes;
                session.state = SessionState::Active;
                session.last_read_at = now_ms;
                self.push_event(
                    &mut guard,
                    EventKind::Refreshed,
                    session_id,
                    &owner,
                    &namespace,
                    &scope,
                    version,
                    bytes,
                    now_ms,
                );
                Ok(RefreshOutcome {
                    snapshot_version: version,
                    previous_version: previous,
                    resident_bytes: bytes,
                })
            }
            Err(err) => {
                if let Some(session) = guard.sessions.get_mut(session_id) {
                    if session.state == SessionState::Refreshing {
                        session.state = SessionState::Active;
                    }
                }
                Err(err)
            }
        }
    }

    pub fn close(&self, session_id: &str, now_ms: i64) -> Result<CloseOutcome, EhError> {
        let mut guard = self.lock()?;
        let session = guard
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| EhError::not_found(format!("session {session_id} not found")))?;
        if !matches!(
            session.state,
            SessionState::Active | SessionState::Refreshing
        ) {
            return Err(ended_error(session.state));
        }
        let released = session.resident_bytes;
        session.state = SessionState::Closed;
        session.release_at = Some(now_ms + self.limits.grace.as_millis() as i64);
        let owner = session.owner.clone();
        let namespace = session.namespace.clone();
        let scope = session.scope_key.clone();
        let version = session.version;
        guard.by_key.retain(|_, id| id != session_id);
        self.push_event(
            &mut guard,
            EventKind::Closed,
            session_id,
            &owner,
            &namespace,
            &scope,
            version,
            released,
            now_ms,
        );
        Ok(CloseOutcome {
            released_bytes: released,
        })
    }

    pub fn info(&self, session_id: &str, now_ms: i64) -> Result<SessionInfo, EhError> {
        let mut guard = self.lock()?;
        self.sweep_locked(&mut guard, now_ms);
        let session = guard
            .sessions
            .get(session_id)
            .ok_or_else(|| EhError::not_found(format!("session {session_id} not found")))?;
        Ok(info_of(session))
    }

    pub fn list(
        &self,
        namespace: Option<&str>,
        owner: Option<&str>,
        now_ms: i64,
    ) -> Result<Vec<SessionInfo>, EhError> {
        let mut guard = self.lock()?;
        self.sweep_locked(&mut guard, now_ms);
        Ok(guard
            .sessions
            .values()
            .filter(|session| namespace.is_none_or(|ns| session.namespace == ns))
            .filter(|session| owner.is_none_or(|owner| session.owner == owner))
            .map(info_of)
            .collect())
    }

    pub fn read<T>(
        &self,
        session_id: &str,
        now_ms: i64,
        f: impl FnOnce(&SessionGraph) -> T,
    ) -> Result<T, EhError> {
        let graph = {
            let mut guard = self.lock()?;
            self.sweep_locked(&mut guard, now_ms);
            let session = guard
                .sessions
                .get_mut(session_id)
                .ok_or_else(|| EhError::not_found(format!("session {session_id} not found")))?;
            if !matches!(
                session.state,
                SessionState::Active | SessionState::Refreshing
            ) {
                return Err(ended_error(session.state));
            }
            if expired(session, now_ms) {
                let owner = session.owner.clone();
                let namespace = session.namespace.clone();
                let scope = session.scope_key.clone();
                let version = session.version;
                let bytes = session.resident_bytes;
                session.state = SessionState::Expired;
                session.release_at = Some(now_ms + self.limits.grace.as_millis() as i64);
                guard.by_key.retain(|_, id| id != session_id);
                self.push_event(
                    &mut guard,
                    EventKind::Expired,
                    session_id,
                    &owner,
                    &namespace,
                    &scope,
                    version,
                    bytes,
                    now_ms,
                );
                return Err(EhError::new(
                    ErrorCode::SessionExpired,
                    format!("session {session_id} expired"),
                ));
            }
            session.last_read_at = now_ms;
            session
                .graph
                .clone()
                .ok_or_else(|| EhError::unavailable("session has no resident graph"))?
        };
        Ok(f(&graph))
    }

    pub fn sweep(&self, now_ms: i64) -> Result<(), EhError> {
        let mut guard = self.lock()?;
        self.sweep_locked(&mut guard, now_ms);
        Ok(())
    }

    pub fn resident_bytes(&self) -> Result<u64, EhError> {
        let guard = self.lock()?;
        Ok(guard.sessions.values().map(|s| s.resident_bytes).sum())
    }

    pub fn counts(&self) -> Result<BTreeMap<String, u64>, EhError> {
        let guard = self.lock()?;
        let mut counts = BTreeMap::new();
        for session in guard.sessions.values() {
            *counts
                .entry(session.state.as_str().to_string())
                .or_insert(0) += 1;
        }
        Ok(counts)
    }

    pub fn loads(&self) -> Result<u64, EhError> {
        Ok(self.lock()?.loads)
    }

    pub fn last_load_ms(&self) -> Result<u64, EhError> {
        Ok(self.lock()?.last_load_ms)
    }

    pub fn evictions(&self) -> Result<BTreeMap<String, u64>, EhError> {
        Ok(self.lock()?.evictions.clone())
    }

    pub fn events(&self) -> Result<Vec<SessionEvent>, EhError> {
        Ok(self.lock()?.events.clone())
    }

    fn accept(
        &self,
        guard: &mut Inner,
        request: &OpenRequest,
        key: &LoadKey,
        graph: SessionGraph,
        load_ms: u64,
    ) -> Result<OpenOutcome, EhError> {
        if graph.resident_bytes > self.limits.per_session_bytes {
            self.push_event(
                guard,
                EventKind::LoadFailed,
                "",
                &request.owner,
                &request.namespace,
                &key.scope,
                graph.version,
                graph.resident_bytes,
                request.now_ms,
            );
            return Err(EhError::new(
                ErrorCode::MemoryBudgetExceeded,
                format!(
                    "scope needs {} bytes which exceeds the per-session cap of {}",
                    graph.resident_bytes, self.limits.per_session_bytes
                ),
            ));
        }
        self.evict_for(guard, graph.resident_bytes, request.now_ms)?;
        let resident: u64 = guard.sessions.values().map(|s| s.resident_bytes).sum();
        if resident + graph.resident_bytes > self.limits.global_bytes {
            self.push_event(
                guard,
                EventKind::LoadFailed,
                "",
                &request.owner,
                &request.namespace,
                &key.scope,
                graph.version,
                graph.resident_bytes,
                request.now_ms,
            );
            return Err(EhError::new(
                ErrorCode::MemoryBudgetExceeded,
                "global session memory budget exceeded",
            ));
        }
        guard.seq += 1;
        let id = format!("gs-{}", guard.seq);
        let idle_ms = request
            .idle_timeout
            .unwrap_or(self.limits.default_idle)
            .as_millis() as i64;
        let max_ms = self.limits.default_max_lifetime.as_millis() as i64;
        let version = graph.version;
        let bytes = graph.resident_bytes;
        guard.loads += 1;
        guard.last_load_ms = load_ms;
        self.push_event(
            guard,
            EventKind::Opened,
            &id,
            &request.owner,
            &request.namespace,
            &key.scope,
            version,
            bytes,
            request.now_ms,
        );
        self.push_event(
            guard,
            EventKind::Loaded,
            &id,
            &request.owner,
            &request.namespace,
            &key.scope,
            version,
            bytes,
            request.now_ms,
        );
        guard.by_key.insert(key.clone(), id.clone());
        guard.sessions.insert(
            id.clone(),
            Session {
                id: id.clone(),
                owner: request.owner.clone(),
                namespace: request.namespace.clone(),
                scope_key: key.scope.clone(),
                version,
                state: SessionState::Active,
                opened_at: request.now_ms,
                last_read_at: request.now_ms,
                idle_ms,
                max_lifetime_ms: max_ms,
                resident_bytes: bytes,
                graph: Some(Arc::new(graph)),
                release_at: None,
            },
        );
        Ok(OpenOutcome {
            graph_session_id: id,
            snapshot_version: version,
            resident_bytes: bytes,
            load_ms,
            reused: false,
        })
    }

    fn evict_for(&self, guard: &mut Inner, needed: u64, now_ms: i64) -> Result<(), EhError> {
        loop {
            let resident: u64 = guard.sessions.values().map(|s| s.resident_bytes).sum();
            if resident + needed <= self.limits.global_bytes {
                return Ok(());
            }
            let victim = guard
                .sessions
                .values()
                .filter(|session| {
                    session.resident_bytes > 0
                        && matches!(session.state, SessionState::Closed | SessionState::Expired)
                })
                .min_by_key(|session| session.release_at.unwrap_or(now_ms))
                .map(|session| session.id.clone());
            let Some(id) = victim else {
                return Ok(());
            };
            self.release_session(guard, &id, now_ms, "budget");
        }
    }

    fn release_session(&self, guard: &mut Inner, id: &str, now_ms: i64, reason: &str) {
        let Some(session) = guard.sessions.get_mut(id) else {
            return;
        };
        let bytes = session.resident_bytes;
        let owner = session.owner.clone();
        let namespace = session.namespace.clone();
        let scope = session.scope_key.clone();
        let version = session.version;
        session.graph = None;
        session.resident_bytes = 0;
        session.state = SessionState::Evicted;
        *guard.evictions.entry(reason.to_string()).or_insert(0) += 1;
        self.push_event(
            guard,
            EventKind::Evicted,
            id,
            &owner,
            &namespace,
            &scope,
            version,
            bytes,
            now_ms,
        );
    }

    fn sweep_locked(&self, guard: &mut Inner, now_ms: i64) {
        let mut expired_ids = Vec::new();
        for session in guard.sessions.values() {
            if matches!(
                session.state,
                SessionState::Active | SessionState::Refreshing
            ) && expired(session, now_ms)
            {
                expired_ids.push(session.id.clone());
            }
        }
        for id in expired_ids {
            if let Some(session) = guard.sessions.get_mut(&id) {
                let owner = session.owner.clone();
                let namespace = session.namespace.clone();
                let scope = session.scope_key.clone();
                let version = session.version;
                let bytes = session.resident_bytes;
                session.state = SessionState::Expired;
                session.release_at = Some(now_ms + self.limits.grace.as_millis() as i64);
                guard.by_key.retain(|_, existing| existing != &id);
                self.push_event(
                    guard,
                    EventKind::Expired,
                    &id,
                    &owner,
                    &namespace,
                    &scope,
                    version,
                    bytes,
                    now_ms,
                );
            }
        }
        let mut release = Vec::new();
        for session in guard.sessions.values() {
            if session.resident_bytes > 0 {
                if let Some(at) = session.release_at {
                    if at <= now_ms
                        && matches!(session.state, SessionState::Closed | SessionState::Expired)
                    {
                        release.push(session.id.clone());
                    }
                }
            }
        }
        for id in release {
            if let Some(session) = guard.sessions.get_mut(&id) {
                session.graph = None;
                session.resident_bytes = 0;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_event(
        &self,
        guard: &mut Inner,
        kind: EventKind,
        session_id: &str,
        owner: &str,
        namespace: &str,
        scope: &str,
        version: u64,
        bytes: u64,
        ts_ms: i64,
    ) {
        tracing::info!(
            session_id,
            owner,
            namespace,
            version,
            bytes,
            event = ?kind,
            "session lifecycle"
        );
        guard.events.push(SessionEvent {
            kind,
            session_id: session_id.to_string(),
            owner: owner.to_string(),
            namespace: namespace.to_string(),
            scope: scope.to_string(),
            version,
            bytes,
            ts_ms,
        });
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, EhError> {
        self.inner
            .lock()
            .map_err(|_| EhError::unavailable("session registry poisoned"))
    }
}

fn reuse(guard: &Inner, key: &LoadKey) -> Option<OpenOutcome> {
    let id = guard.by_key.get(key)?;
    let session = guard.sessions.get(id)?;
    if !matches!(
        session.state,
        SessionState::Active | SessionState::Refreshing
    ) {
        return None;
    }
    Some(OpenOutcome {
        graph_session_id: session.id.clone(),
        snapshot_version: session.version,
        resident_bytes: session.resident_bytes,
        load_ms: 0,
        reused: true,
    })
}

fn expired(session: &Session, now_ms: i64) -> bool {
    now_ms.saturating_sub(session.last_read_at) >= session.idle_ms
        || now_ms.saturating_sub(session.opened_at) >= session.max_lifetime_ms
}

fn ended_error(state: SessionState) -> EhError {
    match state {
        SessionState::Loading => EhError::new(ErrorCode::SessionLoading, "session is loading"),
        SessionState::Failed => EhError::new(ErrorCode::SessionExpired, "session failed to load"),
        SessionState::Closed | SessionState::Expired | SessionState::Evicted => EhError::new(
            ErrorCode::SessionExpired,
            format!("session is {}", state.as_str()),
        ),
        SessionState::Active | SessionState::Refreshing => {
            EhError::unavailable("session is still active")
        }
    }
}

fn info_of(session: &Session) -> SessionInfo {
    let idle_deadline = session.last_read_at + session.idle_ms;
    let life_deadline = session.opened_at + session.max_lifetime_ms;
    SessionInfo {
        graph_session_id: session.id.clone(),
        owner: session.owner.clone(),
        namespace: session.namespace.clone(),
        scope: session.scope_key.clone(),
        state: session.state,
        snapshot_version: session.version,
        resident_bytes: session.resident_bytes,
        opened_at: session.opened_at,
        last_read_at: session.last_read_at,
        expires_at: idle_deadline.min(life_deadline),
    }
}
