//! Minimal Bolt server: lets standard Neo4j drivers run Cypher against ehgraph.
//!
//! Scope (see `docs/wiki/Cypher.md`, "Bolt"):
//! - Handshake negotiates Bolt **5.0** (preferred) or **4.4**. 5.0 carries string
//!   `element_id`s, which are ehgraph's real ids; under 4.4 the integer ids are a
//!   stable hash of the element id.
//! - Messages: HELLO, LOGON/LOGOFF, RUN, PULL, DISCARD, BEGIN, COMMIT, ROLLBACK,
//!   RESET, GOODBYE, TELEMETRY. ROUTE is refused (use `bolt://`, not `neo4j://`).
//! - An auto-commit RUN is one `graph.cypher` call: atomic, one graph version,
//!   serializable (re-run on a lost race).
//! - BEGIN opens a real ehgraph transaction (`graph.tx.*`): RUNs read the snapshot
//!   plus the transaction's own writes, COMMIT validates and writes everything as one
//!   graph version, ROLLBACK / RESET / a failed RUN / a dropped connection discard it.
//!   A lost race at COMMIT fails with `Neo.TransientError.Transaction.Outdated`, which
//!   the drivers' `execute_write` retries. BEGIN `tx_metadata.expect` carries
//!   `{graph_version, versions}` preconditions checked at COMMIT.
//! - Auth: the password (basic) or token (bearer) is the ehgraph bearer token.
//! - Database = ehgraph namespace (`session(database=...)`), else the configured default.
//!
//! Not supported: routing, TLS on this listener, Bolt structs as parameters
//! (temporal/spatial), result streaming beyond in-memory buffering.

use std::collections::VecDeque;

use ehgraph_cypher::Out;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::{Caller, EhError, EhGraph};

const MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];

const HELLO: u8 = 0x01;
const GOODBYE: u8 = 0x02;
const RESET: u8 = 0x0F;
const RUN: u8 = 0x10;
const BEGIN: u8 = 0x11;
const COMMIT: u8 = 0x12;
const ROLLBACK: u8 = 0x13;
const DISCARD: u8 = 0x2F;
const PULL: u8 = 0x3F;
const TELEMETRY: u8 = 0x54;
const ROUTE: u8 = 0x66;
const LOGON: u8 = 0x6A;
const LOGOFF: u8 = 0x6B;

const SUCCESS: u8 = 0x70;
const RECORD: u8 = 0x71;
const IGNORED: u8 = 0x7E;
const FAILURE: u8 = 0x7F;

/// Bolt listener settings.
#[derive(Debug, Clone, Default)]
pub struct BoltOptions {
    /// Namespace used when the client names no database.
    pub default_namespace: Option<String>,
}

/// Accept Bolt connections until the listener fails.
pub async fn serve(
    graph: EhGraph,
    listener: TcpListener,
    options: BoltOptions,
) -> std::io::Result<()> {
    let mut next_id = 0u64;
    loop {
        let (stream, peer) = listener.accept().await?;
        next_id += 1;
        let g = graph.clone();
        let opts = options.clone();
        let id = next_id;
        tokio::spawn(async move {
            if let Err(err) = connection(g, stream, opts, id).await {
                tracing::debug!(%peer, error = %err, "bolt connection closed");
            }
        });
    }
}

// ---------------------------------------------------------------------------
// PackStream

#[derive(Debug, Clone, PartialEq)]
pub enum Pack {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Pack>),
    Map(Vec<(String, Pack)>),
    Struct(u8, Vec<Pack>),
}

impl Pack {
    fn map(entries: Vec<(&str, Pack)>) -> Pack {
        Pack::Map(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    fn get(&self, key: &str) -> Option<&Pack> {
        match self {
            Pack::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Pack::Str(s) => Some(s),
            _ => None,
        }
    }

    fn to_json(&self) -> Result<Value, String> {
        Ok(match self {
            Pack::Null => Value::Null,
            Pack::Bool(b) => json!(b),
            Pack::Int(i) => json!(i),
            Pack::Float(f) => json!(f),
            Pack::Str(s) => json!(s),
            Pack::List(items) => {
                Value::Array(items.iter().map(Pack::to_json).collect::<Result<_, _>>()?)
            }
            Pack::Map(entries) => Value::Object(
                entries
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), v.to_json()?)))
                    .collect::<Result<_, String>>()?,
            ),
            Pack::Bytes(_) => return Err("byte arrays are not supported as parameters".into()),
            Pack::Struct(sig, _) => {
                return Err(format!(
                "structure 0x{sig:02X} (temporal/spatial/graph) is not supported as a parameter"
            ))
            }
        })
    }
}

pub fn encode(value: &Pack, out: &mut Vec<u8>) {
    fn size(out: &mut Vec<u8>, len: usize, tiny: u8, m8: u8) {
        if len < 16 {
            out.push(tiny | len as u8);
        } else if len <= u8::MAX as usize {
            out.push(m8);
            out.push(len as u8);
        } else if len <= u16::MAX as usize {
            out.push(m8 + 1);
            out.extend((len as u16).to_be_bytes());
        } else {
            out.push(m8 + 2);
            out.extend((len as u32).to_be_bytes());
        }
    }
    match value {
        Pack::Null => out.push(0xC0),
        Pack::Bool(false) => out.push(0xC2),
        Pack::Bool(true) => out.push(0xC3),
        Pack::Float(f) => {
            out.push(0xC1);
            out.extend(f.to_be_bytes());
        }
        Pack::Int(i) => {
            let i = *i;
            if (-16..=127).contains(&i) {
                out.push(i as i8 as u8);
            } else if i8::try_from(i).is_ok() {
                out.push(0xC8);
                out.push(i as i8 as u8);
            } else if i16::try_from(i).is_ok() {
                out.push(0xC9);
                out.extend((i as i16).to_be_bytes());
            } else if i32::try_from(i).is_ok() {
                out.push(0xCA);
                out.extend((i as i32).to_be_bytes());
            } else {
                out.push(0xCB);
                out.extend(i.to_be_bytes());
            }
        }
        Pack::Str(s) => {
            let bytes = s.as_bytes();
            if bytes.len() < 16 {
                out.push(0x80 | bytes.len() as u8);
            } else {
                size(out, bytes.len(), 0x80, 0xD0);
            }
            out.extend(bytes);
        }
        Pack::Bytes(b) => {
            if b.len() <= u8::MAX as usize {
                out.push(0xCC);
                out.push(b.len() as u8);
            } else if b.len() <= u16::MAX as usize {
                out.push(0xCD);
                out.extend((b.len() as u16).to_be_bytes());
            } else {
                out.push(0xCE);
                out.extend((b.len() as u32).to_be_bytes());
            }
            out.extend(b);
        }
        Pack::List(items) => {
            size(out, items.len(), 0x90, 0xD4);
            for item in items {
                encode(item, out);
            }
        }
        Pack::Map(entries) => {
            size(out, entries.len(), 0xA0, 0xD8);
            for (k, v) in entries {
                encode(&Pack::Str(k.clone()), out);
                encode(v, out);
            }
        }
        Pack::Struct(sig, fields) => {
            out.push(0xB0 | fields.len() as u8);
            out.push(*sig);
            for f in fields {
                encode(f, out);
            }
        }
    }
}

pub fn decode(buf: &[u8], pos: &mut usize) -> Result<Pack, String> {
    fn take<'a>(buf: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], String> {
        let end = pos
            .checked_add(n)
            .filter(|e| *e <= buf.len())
            .ok_or("truncated packstream")?;
        let s = &buf[*pos..end];
        *pos = end;
        Ok(s)
    }
    fn len(buf: &[u8], pos: &mut usize, width: usize) -> Result<usize, String> {
        let b = take(buf, pos, width)?;
        Ok(match width {
            1 => b[0] as usize,
            2 => u16::from_be_bytes([b[0], b[1]]) as usize,
            _ => u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize,
        })
    }
    let marker = take(buf, pos, 1)?[0];
    let string = |buf: &[u8], pos: &mut usize, n: usize| -> Result<Pack, String> {
        let bytes = take(buf, pos, n)?;
        String::from_utf8(bytes.to_vec())
            .map(Pack::Str)
            .map_err(|_| "invalid utf-8".to_string())
    };
    let list = |buf: &[u8], pos: &mut usize, n: usize| -> Result<Pack, String> {
        let mut items = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            items.push(decode(buf, pos)?);
        }
        Ok(Pack::List(items))
    };
    let map = |buf: &[u8], pos: &mut usize, n: usize| -> Result<Pack, String> {
        let mut entries = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            let Pack::Str(k) = decode(buf, pos)? else {
                return Err("map keys must be strings".into());
            };
            entries.push((k, decode(buf, pos)?));
        }
        Ok(Pack::Map(entries))
    };
    Ok(match marker {
        0x00..=0x7F => Pack::Int(marker as i64),
        0xF0..=0xFF => Pack::Int(marker as i8 as i64),
        0x80..=0x8F => string(buf, pos, (marker & 0x0F) as usize)?,
        0x90..=0x9F => list(buf, pos, (marker & 0x0F) as usize)?,
        0xA0..=0xAF => map(buf, pos, (marker & 0x0F) as usize)?,
        0xB0..=0xBF => {
            let sig = take(buf, pos, 1)?[0];
            let mut fields = Vec::new();
            for _ in 0..(marker & 0x0F) {
                fields.push(decode(buf, pos)?);
            }
            Pack::Struct(sig, fields)
        }
        0xC0 => Pack::Null,
        0xC1 => {
            let b = take(buf, pos, 8)?;
            Pack::Float(f64::from_be_bytes(b.try_into().expect("8 bytes")))
        }
        0xC2 => Pack::Bool(false),
        0xC3 => Pack::Bool(true),
        0xC8 => Pack::Int(take(buf, pos, 1)?[0] as i8 as i64),
        0xC9 => Pack::Int(i16::from_be_bytes(take(buf, pos, 2)?.try_into().expect("2")) as i64),
        0xCA => Pack::Int(i32::from_be_bytes(take(buf, pos, 4)?.try_into().expect("4")) as i64),
        0xCB => Pack::Int(i64::from_be_bytes(
            take(buf, pos, 8)?.try_into().expect("8"),
        )),
        0xCC..=0xCE => {
            let n = len(buf, pos, 1 << (marker - 0xCC))?;
            Pack::Bytes(take(buf, pos, n)?.to_vec())
        }
        0xD0..=0xD2 => {
            let n = len(buf, pos, 1 << (marker - 0xD0))?;
            string(buf, pos, n)?
        }
        0xD4..=0xD6 => {
            let n = len(buf, pos, 1 << (marker - 0xD4))?;
            list(buf, pos, n)?
        }
        0xD8..=0xDA => {
            let n = len(buf, pos, 1 << (marker - 0xD8))?;
            map(buf, pos, n)?
        }
        other => return Err(format!("unknown packstream marker 0x{other:02X}")),
    })
}

// ---------------------------------------------------------------------------
// Connection

/// Pick the first client proposal we support: 5.0, else 4.4.
pub fn negotiate(proposals: &[u8; 16]) -> Option<(u8, u8)> {
    for p in proposals.chunks(4) {
        let (range, minor, major) = (p[1], p[2], p[3]);
        let lowest = minor.saturating_sub(range);
        if major == 5 && lowest == 0 {
            return Some((5, 0));
        }
        if major == 4 && (lowest..=minor).contains(&4) {
            return Some((4, 4));
        }
    }
    None
}

struct Pending {
    rows: VecDeque<Vec<Pack>>,
    summary: Vec<(&'static str, Pack)>,
}

/// An open explicit transaction. Dropping it (RESET, ROLLBACK, GOODBYE, a dropped
/// connection) rolls it back; after COMMIT the rollback is a no-op.
struct Tx {
    db: String,
    tx_id: String,
    expect: crate::Expect,
    caller: Caller,
    graph: EhGraph,
}

impl Drop for Tx {
    fn drop(&mut self) {
        let _ = self.graph.tx_rollback_as(&self.caller, &self.tx_id);
    }
}

async fn read_message<R: AsyncRead + Unpin>(stream: &mut R) -> std::io::Result<Vec<u8>> {
    let mut msg = Vec::new();
    loop {
        let mut header = [0u8; 2];
        stream.read_exact(&mut header).await?;
        let n = u16::from_be_bytes(header) as usize;
        if n == 0 {
            if msg.is_empty() {
                continue; // NOOP keep-alive chunk
            }
            return Ok(msg);
        }
        let start = msg.len();
        msg.resize(start + n, 0);
        stream.read_exact(&mut msg[start..]).await?;
    }
}

async fn send<W: AsyncWrite + Unpin>(
    stream: &mut W,
    sig: u8,
    fields: Vec<Pack>,
) -> std::io::Result<()> {
    let mut body = Vec::new();
    encode(&Pack::Struct(sig, fields), &mut body);
    let mut framed = Vec::with_capacity(body.len() + 8);
    for chunk in body.chunks(u16::MAX as usize) {
        framed.extend((chunk.len() as u16).to_be_bytes());
        framed.extend(chunk);
    }
    framed.extend([0, 0]);
    stream.write_all(&framed).await
}

async fn success<W: AsyncWrite + Unpin>(
    stream: &mut W,
    meta: Vec<(&str, Pack)>,
) -> std::io::Result<()> {
    send(stream, SUCCESS, vec![Pack::map(meta)]).await
}

async fn failure<W: AsyncWrite + Unpin>(
    stream: &mut W,
    code: &str,
    message: &str,
) -> std::io::Result<()> {
    send(
        stream,
        FAILURE,
        vec![Pack::map(vec![
            ("code", Pack::Str(code.into())),
            ("message", Pack::Str(message.into())),
        ])],
    )
    .await
}

fn neo4j_code(err: &EhError) -> &'static str {
    if let Some(kind) = ehgraph_cypher::kind_of_message(&err.message) {
        return kind.neo4j_code();
    }
    match err.code {
        crate::ErrorCode::Unauthorized => "Neo.ClientError.Security.Unauthorized",
        crate::ErrorCode::NotFound if err.message.starts_with("transaction ") => {
            "Neo.ClientError.Transaction.TransactionNotFound"
        }
        crate::ErrorCode::NotFound => "Neo.ClientError.Database.DatabaseNotFound",
        crate::ErrorCode::Unavailable | crate::ErrorCode::MemoryBudgetExceeded => {
            "Neo.TransientError.General.DatabaseUnavailable"
        }
        // A lost race: drivers retry transient errors (`execute_write`).
        crate::ErrorCode::VersionConflict if err.message.starts_with("serialization conflict") => {
            "Neo.TransientError.Transaction.Outdated"
        }
        crate::ErrorCode::VersionConflict => "Neo.ClientError.Schema.ConstraintValidationFailed",
        crate::ErrorCode::Conflict => "Neo.ClientError.Schema.ConstraintValidationFailed",
        _ => "Neo.ClientError.Statement.ArgumentError",
    }
}

fn token_of(auth: &Pack) -> String {
    match auth.get("scheme").and_then(Pack::as_str) {
        Some("basic") | Some("bearer") => auth
            .get("credentials")
            .and_then(Pack::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

async fn connection<S: AsyncRead + AsyncWrite + Unpin>(
    graph: EhGraph,
    mut stream: S,
    options: BoltOptions,
    connection_id: u64,
) -> std::io::Result<()> {
    let mut magic = [0u8; 4];
    stream.read_exact(&mut magic).await?;
    if magic != MAGIC {
        return Ok(());
    }
    let mut proposals = [0u8; 16];
    stream.read_exact(&mut proposals).await?;
    let Some((major, minor)) = negotiate(&proposals) else {
        stream.write_all(&[0, 0, 0, 0]).await?;
        return Ok(());
    };
    stream.write_all(&[0, 0, minor, major]).await?;
    let v5 = major == 5;
    let mut caller: Option<Caller> = None;
    let mut failed = false;
    let mut pending: Option<Pending> = None;
    let mut tx: Option<Tx> = None;
    loop {
        let bytes = read_message(&mut stream).await?;
        let mut pos = 0;
        let (sig, fields) = match decode(&bytes, &mut pos) {
            Ok(Pack::Struct(sig, fields)) => (sig, fields),
            _ => {
                failure(
                    &mut stream,
                    "Neo.ClientError.Request.InvalidFormat",
                    "expected a Bolt message",
                )
                .await?;
                return Ok(());
            }
        };
        if sig == GOODBYE {
            return Ok(());
        }
        if sig == RESET {
            failed = false;
            pending = None;
            tx = None;
            success(&mut stream, vec![]).await?;
            continue;
        }
        if failed {
            send(&mut stream, IGNORED, vec![]).await?;
            continue;
        }
        let arg = |i: usize| fields.get(i).cloned().unwrap_or(Pack::Null);
        match sig {
            HELLO | LOGON => {
                let extra = arg(0);
                let token = token_of(&extra);
                // Bolt 5.0 carries auth in HELLO; a LOGON (5.1+) would too.
                match graph.authorize(&token) {
                    Ok(c) => {
                        caller = Some(c);
                        let mut meta = vec![];
                        if sig == HELLO {
                            // Drivers refuse an agent that does not start with "Neo4j/";
                            // Bolt-compatible servers (e.g. Memgraph) announce compatibility
                            // the same way and name themselves after it.
                            meta.push((
                                "server",
                                Pack::Str(format!("Neo4j/5.0.0 compatible graph database server - ehgraph (build {})", graph.build())),
                            ));
                            meta.push((
                                "connection_id",
                                Pack::Str(format!("bolt-{connection_id}")),
                            ));
                            meta.push(("hints", Pack::Map(vec![])));
                        }
                        success(&mut stream, meta).await?;
                    }
                    Err(err) => {
                        failure(
                            &mut stream,
                            "Neo.ClientError.Security.Unauthorized",
                            &err.message,
                        )
                        .await?;
                        return Ok(());
                    }
                }
            }
            LOGOFF => {
                caller = None;
                success(&mut stream, vec![]).await?;
            }
            TELEMETRY => success(&mut stream, vec![]).await?,
            ROUTE => {
                failed = true;
                failure(
                    &mut stream,
                    "Neo.ClientError.Request.Invalid",
                    "ehgraph does not serve routing tables; connect with bolt:// instead of neo4j://",
                )
                .await?;
            }
            BEGIN => {
                let Some(c) = caller.clone() else {
                    failed = true;
                    failure(
                        &mut stream,
                        "Neo.ClientError.Security.Unauthorized",
                        "HELLO first",
                    )
                    .await?;
                    continue;
                };
                let extra = arg(0);
                let db = extra
                    .get("db")
                    .and_then(Pack::as_str)
                    .map(str::to_string)
                    .or_else(|| options.default_namespace.clone());
                let Some(db) = db else {
                    failed = true;
                    failure(
                        &mut stream,
                        "Neo.ClientError.Database.DatabaseNotFound",
                        "no database given and no default namespace configured; open the session with database=<namespace>",
                    )
                    .await?;
                    continue;
                };
                let meta = extra.get("tx_metadata").map(Pack::to_json);
                let expect = match meta {
                    Some(Ok(m)) => match m.get("expect") {
                        Some(e) => serde_json::from_value::<crate::Expect>(e.clone())
                            .map_err(|err| format!("tx_metadata.expect: {err}")),
                        None => Ok(Default::default()),
                    },
                    Some(Err(msg)) => Err(msg),
                    None => Ok(Default::default()),
                };
                let expect = match expect {
                    Ok(e) => e,
                    Err(msg) => {
                        failed = true;
                        failure(&mut stream, "Neo.ClientError.Request.Invalid", &msg).await?;
                        continue;
                    }
                };
                let timeout_ms = match extra.get("tx_timeout") {
                    Some(Pack::Int(ms)) if *ms > 0 => {
                        Some((*ms).min(ehgraph_api::MAX_TX_TIMEOUT_MS))
                    }
                    _ => None,
                };
                let opts = crate::TxBegin {
                    namespace: db.clone(),
                    timeout_ms,
                    source: None,
                    execution_id: None,
                };
                let g = graph.clone();
                let c2 = c.clone();
                let begun = tokio::task::spawn_blocking(move || g.tx_begin_as(&c2, opts))
                    .await
                    .map_err(|err| EhError::unavailable(format!("engine task failed: {err}")))
                    .and_then(|r| r);
                match begun {
                    Ok(info) => {
                        tx = Some(Tx {
                            db,
                            tx_id: info.tx_id,
                            expect,
                            caller: c,
                            graph: graph.clone(),
                        });
                        success(&mut stream, vec![]).await?;
                    }
                    Err(err) => {
                        failed = true;
                        failure(&mut stream, neo4j_code(&err), &err.message).await?;
                    }
                }
            }
            COMMIT => {
                let Some(t) = tx.take() else {
                    failed = true;
                    failure(
                        &mut stream,
                        "Neo.ClientError.Request.Invalid",
                        "COMMIT without BEGIN",
                    )
                    .await?;
                    continue;
                };
                let g = graph.clone();
                let (c, id, expect, db) = (
                    t.caller.clone(),
                    t.tx_id.clone(),
                    t.expect.clone(),
                    t.db.clone(),
                );
                let done = tokio::task::spawn_blocking(move || g.tx_commit_as(&c, &id, &expect))
                    .await
                    .map_err(|err| EhError::unavailable(format!("engine task failed: {err}")))
                    .and_then(|r| r);
                drop(t);
                match done {
                    Ok(done) => {
                        success(
                            &mut stream,
                            vec![(
                                "bookmark",
                                Pack::Str(format!("ehgraph:{db}:{}", done.graph_version)),
                            )],
                        )
                        .await?;
                    }
                    Err(err) => {
                        failed = true;
                        failure(&mut stream, neo4j_code(&err), &err.message).await?;
                    }
                }
            }
            ROLLBACK => {
                // Dropping the transaction rolls it back.
                tx = None;
                success(&mut stream, vec![]).await?;
            }
            RUN => {
                let Some(c) = caller.clone() else {
                    failed = true;
                    failure(
                        &mut stream,
                        "Neo.ClientError.Security.Unauthorized",
                        "HELLO first",
                    )
                    .await?;
                    continue;
                };
                let query = arg(0).as_str().unwrap_or("").to_string();
                let params = match arg(1) {
                    Pack::Null => Ok(json!({})),
                    p => p.to_json(),
                };
                let params = match params {
                    Ok(p) => p,
                    Err(msg) => {
                        failed = true;
                        failure(&mut stream, "Neo.ClientError.Statement.TypeError", &msg).await?;
                        continue;
                    }
                };
                let extra = arg(2);
                let db = tx
                    .as_ref()
                    .map(|t| t.db.clone())
                    .or_else(|| extra.get("db").and_then(Pack::as_str).map(str::to_string))
                    .or_else(|| options.default_namespace.clone());
                let Some(db) = db else {
                    failed = true;
                    failure(
                        &mut stream,
                        "Neo.ClientError.Database.DatabaseNotFound",
                        "no database given and no default namespace configured; open the session with database=<namespace>",
                    )
                    .await?;
                    continue;
                };
                let g = graph.clone();
                let in_tx = tx.as_ref().map(|t| t.tx_id.clone());
                let db_arg = db.clone();
                let result = tokio::task::spawn_blocking(move || match in_tx {
                    Some(tx_id) => {
                        let params = params.as_object().cloned().unwrap_or_default();
                        g.tx_run_as(&c, &tx_id, &query, &params)
                    }
                    None => {
                        let args = json!({"namespace": db_arg, "query": query, "params": params});
                        g.cypher_as(&c, &args)
                    }
                })
                .await
                .map_err(|err| EhError::unavailable(format!("engine task failed: {err}")))
                .and_then(|r| r);
                match result {
                    Ok(run) => {
                        let stats = &run.outcome.stats;
                        let fields: Vec<Pack> =
                            run.outcome.columns.iter().cloned().map(Pack::Str).collect();
                        let rows = run
                            .outcome
                            .rows
                            .iter()
                            .map(|row| row.iter().map(|v| to_pack(v, v5)).collect())
                            .collect();
                        let mut summary = vec![
                            (
                                "bookmark",
                                Pack::Str(format!("ehgraph:{db}:{}", run.graph_version)),
                            ),
                            (
                                "type",
                                Pack::Str(if stats.contains_updates() { "rw" } else { "r" }.into()),
                            ),
                            ("t_last", Pack::Int(0)),
                            ("db", Pack::Str(db.clone())),
                        ];
                        if stats.contains_updates() {
                            summary.push((
                                "stats",
                                Pack::map(vec![
                                    ("nodes-created", Pack::Int(stats.nodes_created as i64)),
                                    ("nodes-deleted", Pack::Int(stats.nodes_deleted as i64)),
                                    (
                                        "relationships-created",
                                        Pack::Int(stats.relationships_created as i64),
                                    ),
                                    (
                                        "relationships-deleted",
                                        Pack::Int(stats.relationships_deleted as i64),
                                    ),
                                    ("properties-set", Pack::Int(stats.properties_set as i64)),
                                ]),
                            ));
                        }
                        pending = Some(Pending { rows, summary });
                        let mut meta =
                            vec![("fields", Pack::List(fields)), ("t_first", Pack::Int(0))];
                        if tx.is_some() {
                            meta.push(("qid", Pack::Int(0)));
                        }
                        success(&mut stream, meta).await?;
                    }
                    Err(err) => {
                        failed = true;
                        failure(&mut stream, neo4j_code(&err), &err.message).await?;
                    }
                }
            }
            PULL | DISCARD => {
                let Some(p) = pending.as_mut() else {
                    failed = true;
                    failure(
                        &mut stream,
                        "Neo.ClientError.Request.Invalid",
                        "no result to consume",
                    )
                    .await?;
                    continue;
                };
                let n = match arg(0).get("n") {
                    Some(Pack::Int(n)) if *n >= 0 => *n as usize,
                    _ => usize::MAX,
                };
                if sig == DISCARD {
                    p.rows.clear();
                }
                let mut sent = 0;
                while sent < n {
                    let Some(row) = p.rows.pop_front() else { break };
                    send(&mut stream, RECORD, vec![Pack::List(row)]).await?;
                    sent += 1;
                }
                if p.rows.is_empty() {
                    let mut summary = pending.take().expect("pending").summary;
                    if tx.is_some() {
                        summary.retain(|(k, _)| *k != "bookmark");
                    }
                    success(&mut stream, summary).await?;
                } else {
                    success(&mut stream, vec![("has_more", Pack::Bool(true))]).await?;
                }
            }
            other => {
                failed = true;
                failure(
                    &mut stream,
                    "Neo.ClientError.Request.Invalid",
                    &format!("unsupported Bolt message 0x{other:02X}"),
                )
                .await?;
            }
        }
    }
}

/// Stable non-negative integer id for Bolt's legacy integer ids.
fn int_id(id: &str) -> i64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h & (i64::MAX as u64)) as i64
}

fn props_pack(props: &std::collections::BTreeMap<String, Out>, v5: bool) -> Pack {
    Pack::Map(
        props
            .iter()
            .map(|(k, v)| (k.clone(), to_pack(v, v5)))
            .collect(),
    )
}

pub fn to_pack(v: &Out, v5: bool) -> Pack {
    match v {
        Out::Null => Pack::Null,
        Out::Bool(b) => Pack::Bool(*b),
        Out::Int(i) => Pack::Int(*i),
        Out::Float(f) => Pack::Float(*f),
        Out::Str(s) => Pack::Str(s.clone()),
        Out::Ts(_) => match v.to_json() {
            Value::Object(m) => Pack::Str(
                m.get("$timestamp")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ),
            _ => Pack::Null,
        },
        Out::List(items) => Pack::List(items.iter().map(|x| to_pack(x, v5)).collect()),
        Out::Map(m) => props_pack(m, v5),
        Out::Node { id, labels, props } => {
            let mut f = vec![
                Pack::Int(int_id(id)),
                Pack::List(labels.iter().cloned().map(Pack::Str).collect()),
                props_pack(props, v5),
            ];
            if v5 {
                f.push(Pack::Str(id.clone()));
            }
            Pack::Struct(0x4E, f)
        }
        Out::Rel {
            id,
            type_name,
            start,
            end,
            props,
        } => {
            let mut f = vec![
                Pack::Int(int_id(id)),
                Pack::Int(int_id(start)),
                Pack::Int(int_id(end)),
                Pack::Str(type_name.clone()),
                props_pack(props, v5),
            ];
            if v5 {
                f.extend([
                    Pack::Str(id.clone()),
                    Pack::Str(start.clone()),
                    Pack::Str(end.clone()),
                ]);
            }
            Pack::Struct(0x52, f)
        }
        Out::Path { nodes, rels } => {
            let node_ids: Vec<&str> = nodes
                .iter()
                .map(|n| match n {
                    Out::Node { id, .. } => id.as_str(),
                    _ => "",
                })
                .collect();
            let mut uniq_nodes: Vec<&Out> = Vec::new();
            let mut uniq_node_ids: Vec<&str> = Vec::new();
            for (n, id) in nodes.iter().zip(&node_ids) {
                if !uniq_node_ids.contains(id) {
                    uniq_node_ids.push(id);
                    uniq_nodes.push(n);
                }
            }
            let mut uniq_rels: Vec<Pack> = Vec::new();
            let mut uniq_rel_ids: Vec<&str> = Vec::new();
            let mut indices = Vec::new();
            for (i, r) in rels.iter().enumerate() {
                let Out::Rel {
                    id,
                    type_name,
                    start,
                    props,
                    ..
                } = r
                else {
                    continue;
                };
                let pos = match uniq_rel_ids.iter().position(|x| x == id) {
                    Some(p) => p,
                    None => {
                        uniq_rel_ids.push(id);
                        let mut f = vec![
                            Pack::Int(int_id(id)),
                            Pack::Str(type_name.clone()),
                            props_pack(props, v5),
                        ];
                        if v5 {
                            f.push(Pack::Str(id.clone()));
                        }
                        uniq_rels.push(Pack::Struct(0x72, f));
                        uniq_rels.len() - 1
                    }
                } as i64
                    + 1;
                let forward = node_ids.get(i).is_some_and(|prev| prev == start);
                indices.push(Pack::Int(if forward { pos } else { -pos }));
                let next = node_ids.get(i + 1).copied().unwrap_or("");
                let ni = uniq_node_ids.iter().position(|x| *x == next).unwrap_or(0);
                indices.push(Pack::Int(ni as i64));
            }
            Pack::Struct(
                0x50,
                vec![
                    Pack::List(uniq_nodes.into_iter().map(|n| to_pack(n, v5)).collect()),
                    Pack::List(uniq_rels),
                    Pack::List(indices),
                ],
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packstream_roundtrip() {
        let value = Pack::map(vec![
            ("a", Pack::Int(-17)),
            ("b", Pack::Int(70_000)),
            ("c", Pack::Str("x".repeat(300))),
            ("d", Pack::List((0..20).map(Pack::Int).collect())),
            ("e", Pack::Float(1.5)),
            (
                "f",
                Pack::Struct(0x4E, vec![Pack::Int(1), Pack::Null, Pack::Bool(true)]),
            ),
        ]);
        let mut buf = Vec::new();
        encode(&value, &mut buf);
        let mut pos = 0;
        assert_eq!(decode(&buf, &mut pos).unwrap(), value);
        assert_eq!(pos, buf.len());
    }

    #[test]
    fn negotiates_five_zero_then_four_four() {
        // 5.4 with range 4 (5.0..=5.4), then 4.4 range 2.
        let p = [0, 4, 4, 5, 0, 2, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(negotiate(&p), Some((5, 0)));
        let p = [0, 0, 4, 5, 0, 2, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(negotiate(&p), Some((4, 4)));
        assert_eq!(negotiate(&[0; 16]), None);
    }
}
