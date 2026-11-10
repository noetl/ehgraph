//! Query execution over one read view plus a copy-on-write write overlay.

use std::cell::Cell;
use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::hash::{BuildHasher, Hasher};

use ehgraph_core::{
    default_edge_id, validate_id, ActiveFilter, Edge, EdgeInput, Node, NodeInput, PropValue,
};
use ehgraph_memory::SessionGraph;
use serde_json::{json, Value as Json};

use crate::ast::*;
use crate::value::{compare, equals, key, order, Val};
use crate::CypherError;

type Row = BTreeMap<String, Val>;
type PropMap = BTreeMap<String, Val>;

#[derive(Debug, Clone)]
pub struct ExecOptions {
    /// `source` stamped on elements this query writes.
    pub source: String,
    pub execution_id: String,
    pub now_ms: i64,
    /// Cap on variable-length expansion and shortestPath depth.
    pub max_hops: u32,
    /// Cap on intermediate and result rows.
    pub max_rows: usize,
    /// Which nodes MATCH may bind (`active` by default, like `graph.query`).
    pub visibility: ActiveFilter,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            source: "cypher".into(),
            execution_id: "cypher".into(),
            now_ms: 0,
            max_hops: 16,
            max_rows: 100_000,
            visibility: ActiveFilter::Active,
        }
    }
}

/// Update counters, named like Neo4j's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub nodes_created: u64,
    pub nodes_deleted: u64,
    pub relationships_created: u64,
    pub relationships_deleted: u64,
    pub properties_set: u64,
}

impl Stats {
    pub fn contains_updates(&self) -> bool {
        self != &Stats::default()
    }

    pub fn to_json(&self) -> Json {
        json!({
            "contains_updates": self.contains_updates(),
            "nodes_created": self.nodes_created,
            "nodes_deleted": self.nodes_deleted,
            "relationships_created": self.relationships_created,
            "relationships_deleted": self.relationships_deleted,
            "properties_set": self.properties_set,
        })
    }
}

/// Net effect of a query, as the existing write operations express it.
#[derive(Debug, Clone, Default)]
pub struct Changes {
    pub nodes: Vec<NodeInput>,
    pub edges: Vec<EdgeInput>,
    /// Element ids to soft-deactivate (`graph.deactivate` by ids).
    pub deactivate: Vec<String>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty() && self.deactivate.is_empty()
    }
}

/// A result value with graph elements resolved (so a renderer needs no graph).
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Ts(i64),
    List(Vec<Out>),
    Map(BTreeMap<String, Out>),
    Node {
        id: String,
        labels: Vec<String>,
        props: BTreeMap<String, Out>,
    },
    Rel {
        id: String,
        type_name: String,
        start: String,
        end: String,
        props: BTreeMap<String, Out>,
    },
    Path {
        nodes: Vec<Out>,
        rels: Vec<Out>,
    },
}

impl Out {
    /// JSON shaped like the Neo4j Query API's plain-JSON values.
    pub fn to_json(&self) -> Json {
        let map = |m: &BTreeMap<String, Out>| -> Json {
            Json::Object(m.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
        };
        match self {
            Out::Null => Json::Null,
            Out::Bool(b) => json!(b),
            Out::Int(i) => json!(i),
            Out::Float(f) => serde_json::Number::from_f64(*f)
                .map(Json::Number)
                .unwrap_or(Json::Null),
            Out::Str(s) => json!(s),
            Out::Ts(t) => PropValue::Timestamp(*t).to_json(),
            Out::List(items) => Json::Array(items.iter().map(Out::to_json).collect()),
            Out::Map(m) => map(m),
            Out::Node { id, labels, props } => json!({
                "elementId": id,
                "labels": labels,
                "properties": map(props),
            }),
            Out::Rel {
                id,
                type_name,
                start,
                end,
                props,
            } => json!({
                "elementId": id,
                "type": type_name,
                "startNodeElementId": start,
                "endNodeElementId": end,
                "properties": map(props),
            }),
            Out::Path { nodes, rels } => json!({
                "nodes": nodes.iter().map(Out::to_json).collect::<Vec<_>>(),
                "relationships": rels.iter().map(Out::to_json).collect::<Vec<_>>(),
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Out>>,
    pub stats: Stats,
    pub changes: Changes,
}

impl Outcome {
    pub fn rows_json(&self) -> Vec<Json> {
        self.rows
            .iter()
            .map(|row| Json::Array(row.iter().map(Out::to_json).collect()))
            .collect()
    }
}

/// Execute `query` against `graph`. Never writes: the net changes are returned.
pub fn execute(
    graph: &SessionGraph,
    query: &Query,
    params: &BTreeMap<String, Val>,
    opts: &ExecOptions,
) -> Result<Outcome, CypherError> {
    let mut state = TxState::default();
    execute_with(graph, &mut state, query, params, opts, query.is_updating())
}

/// Execute one statement of a transaction against `graph` (the transaction's
/// snapshot) plus the writes of the transaction's earlier statements, which live in
/// `state`. The statement's own writes are added to `state`, and everything it read
/// is added to `state.footprint` for commit-time validation.
///
/// `Outcome::changes` is the net effect of the whole transaction so far. After an
/// error, `state` may hold part of the failed statement: callers must not commit it
/// (a failed statement fails its transaction, as in Neo4j).
pub fn execute_in(
    graph: &SessionGraph,
    state: &mut TxState,
    query: &Query,
    params: &BTreeMap<String, Val>,
    opts: &ExecOptions,
) -> Result<Outcome, CypherError> {
    execute_with(graph, state, query, params, opts, true)
}

fn execute_with(
    graph: &SessionGraph,
    state: &mut TxState,
    query: &Query,
    params: &BTreeMap<String, Val>,
    opts: &ExecOptions,
    track: bool,
) -> Result<Outcome, CypherError> {
    if query.is_updating() && opts.visibility != ActiveFilter::Active {
        return Err(CypherError::semantic(
            "updating queries run over active elements only; drop the `active` filter",
        ));
    }
    let overlay = std::mem::take(&mut state.overlay);
    let footprint = std::mem::take(&mut state.footprint);
    let mut ex = Exec {
        w: Work::with(graph, overlay, footprint, track),
        params,
        opts,
        stats: Stats::default(),
        budget: Cell::new(opts.max_rows.saturating_mul(10).max(1000)),
        rand: RandomState::new(),
        seq: 0,
        rng: Cell::new(0),
        pending_delete_checks: Vec::new(),
    };
    let result = run(&mut ex, query);
    let Work { ov, fp, .. } = ex.w;
    state.overlay = ov;
    state.footprint = fp.into_inner();
    result
}

fn run(ex: &mut Exec<'_, '_>, query: &Query) -> Result<Outcome, CypherError> {
    let (columns, mut rows, returned) = ex.run_clauses(&query.clauses)?;
    for (all, part) in &query.unions {
        let (cols, more, _) = ex.run_clauses(part)?;
        if cols != columns {
            return Err(CypherError::semantic(
                "all queries in a UNION must return the same column names",
            ));
        }
        rows.extend(more);
        if !*all {
            let mut seen = HashSet::new();
            rows.retain(|row| {
                let k: Vec<String> = columns
                    .iter()
                    .map(|c| key(row.get(c).unwrap_or(&Val::Null)))
                    .collect();
                seen.insert(k)
            });
        }
        ex.check_rows(rows.len())?;
    }
    ex.verify_deletes()?;
    let out_rows = if returned {
        rows.iter()
            .map(|row| {
                columns
                    .iter()
                    .map(|c| ex.resolve(row.get(c).unwrap_or(&Val::Null)))
                    .collect()
            })
            .collect()
    } else {
        Vec::new()
    };
    let changes = ex.w.changes();
    Ok(Outcome {
        columns,
        rows: out_rows,
        stats: std::mem::take(&mut ex.stats),
        changes,
    })
}

impl Exec<'_, '_> {
    /// Run one single query (no UNION): returns its columns, rows, and whether it
    /// ended in RETURN.
    fn run_clauses(
        &mut self,
        clauses: &[Clause],
    ) -> Result<(Vec<String>, Vec<Row>, bool), CypherError> {
        let mut rows: Vec<Row> = vec![Row::new()];
        let mut columns = Vec::new();
        let mut returned = false;
        for clause in clauses {
            rows = match clause {
                Clause::Match {
                    optional,
                    patterns,
                    where_,
                } => self.match_clause(rows, *optional, patterns, where_.as_ref())?,
                Clause::Unwind { expr, alias } => {
                    let mut out = Vec::new();
                    for row in rows {
                        match self.eval(expr, &row, None)? {
                            Val::Null => {}
                            Val::List(items) => {
                                for item in items {
                                    let mut r = row.clone();
                                    r.insert(alias.clone(), item);
                                    out.push(r);
                                }
                            }
                            other => {
                                let mut r = row.clone();
                                r.insert(alias.clone(), other);
                                out.push(r);
                            }
                        }
                        self.check_rows(out.len())?;
                    }
                    out
                }
                Clause::With(p) => self.project(rows, p)?.1,
                Clause::Return(p) => {
                    let (cols, out) = self.project(rows, p)?;
                    columns = cols;
                    returned = true;
                    out
                }
                Clause::Create(parts) => {
                    let mut out = Vec::with_capacity(rows.len());
                    for row in rows {
                        let mut r = row;
                        for part in parts {
                            r = self.create_part(part, r, false)?;
                        }
                        out.push(r);
                    }
                    out
                }
                Clause::Merge {
                    part,
                    on_create,
                    on_match,
                } => {
                    let mut out = Vec::new();
                    for row in rows {
                        let found = self.match_part(part, &row, &[])?;
                        if found.is_empty() {
                            let mut r = self.create_part(part, row, true)?;
                            self.apply_set(on_create, &mut r)?;
                            out.push(r);
                        } else {
                            for (mut r, _) in found {
                                self.apply_set(on_match, &mut r)?;
                                out.push(r);
                            }
                        }
                        self.check_rows(out.len())?;
                    }
                    out
                }
                Clause::Set(items) => {
                    let mut out = Vec::with_capacity(rows.len());
                    for mut row in rows {
                        self.apply_set(items, &mut row)?;
                        out.push(row);
                    }
                    out
                }
                Clause::Remove(items) => {
                    for row in &rows {
                        for item in items {
                            match item {
                                RemoveItem::Prop { var, key } => {
                                    let target = self.lookup(var, row)?;
                                    self.set_prop(&target, key, Val::Null)?;
                                }
                                RemoveItem::Labels { .. } => {
                                    return Err(CypherError::unsupported(
                                        "REMOVE n:Label (ehgraph nodes carry exactly one label)",
                                    ))
                                }
                            }
                        }
                    }
                    rows
                }
                Clause::Delete { detach, exprs } => {
                    for row in &rows {
                        for e in exprs {
                            let v = self.eval(e, row, None)?;
                            self.delete(v, *detach)?;
                        }
                    }
                    rows
                }
            };
            self.check_rows(rows.len())?;
        }
        Ok((columns, rows, returned))
    }
}

// ---------------------------------------------------------------------------
// Working graph: base view + overlay images.

/// Writes of a transaction so far: full images of changed elements over the base
/// view. Owned, so it survives from one statement of a transaction to the next.
#[derive(Debug, Clone, Default)]
pub struct Overlay {
    nodes: HashMap<String, Node>,
    edges: HashMap<String, Edge>,
    new_nodes: Vec<String>,
    extra_out: HashMap<String, Vec<String>>,
    extra_in: HashMap<String, Vec<String>>,
    dirty_nodes: Vec<String>,
    dirty_node_set: HashSet<String>,
    dirty_edges: Vec<String>,
    dirty_edge_set: HashSet<String>,
}

/// What a transaction read (and wrote) from its snapshot, at the granularity
/// commit-time validation needs. A commit is serializable when no element changed
/// after the snapshot intersects the footprint:
///
/// - `nodes` / `edges`: element ids looked up (present or not) or written;
/// - `labels`: labels scanned (catches nodes created or relabelled into a scan);
/// - `out_of` / `in_of`: nodes whose outgoing / incoming adjacency was read
///   (catches edges created, changed or removed at those nodes);
/// - `all_nodes`: a label-less node scan (any node change conflicts).
#[derive(Debug, Clone, Default)]
pub struct Footprint {
    pub nodes: HashSet<String>,
    pub edges: HashSet<String>,
    pub labels: HashSet<String>,
    pub out_of: HashSet<String>,
    pub in_of: HashSet<String>,
    pub all_nodes: bool,
}

impl Footprint {
    pub fn is_empty(&self) -> bool {
        !self.all_nodes
            && self.nodes.is_empty()
            && self.edges.is_empty()
            && self.labels.is_empty()
            && self.out_of.is_empty()
            && self.in_of.is_empty()
    }

    /// The first element changed after `since` (its `version_updated > since`)
    /// that this footprint depends on, described for an error message.
    pub fn conflict<'a>(
        &self,
        since: u64,
        nodes: impl IntoIterator<Item = &'a Node>,
        edges: impl IntoIterator<Item = &'a Edge>,
    ) -> Option<String> {
        for n in nodes {
            if n.version_updated > since
                && (self.all_nodes
                    || self.nodes.contains(&n.id)
                    || self.labels.contains(&n.type_name))
            {
                return Some(format!(
                    "node {} ({}) changed at version {} after this transaction's snapshot {}",
                    n.id, n.type_name, n.version_updated, since
                ));
            }
        }
        for e in edges {
            if e.version_updated > since
                && (self.edges.contains(&e.id)
                    || self.out_of.contains(&e.src)
                    || self.in_of.contains(&e.dst))
            {
                return Some(format!(
                    "relationship {} changed at version {} after this transaction's snapshot {}",
                    e.id, e.version_updated, since
                ));
            }
        }
        None
    }
}

/// The per-transaction state `execute_in` carries between statements.
#[derive(Debug, Clone, Default)]
pub struct TxState {
    overlay: Overlay,
    pub footprint: Footprint,
}

impl TxState {
    /// Net effect of the transaction against its snapshot `base`.
    pub fn changes(&self, base: &SessionGraph) -> Changes {
        let w = Work::with(base, self.overlay.clone(), Footprint::default(), false);
        w.changes()
    }

    /// Whether any statement wrote (the net effect may still be empty).
    pub fn has_writes(&self) -> bool {
        !self.overlay.dirty_nodes.is_empty() || !self.overlay.dirty_edges.is_empty()
    }
}

struct Work<'g> {
    base: &'g SessionGraph,
    ov: Overlay,
    fp: std::cell::RefCell<Footprint>,
    track: bool,
}

impl<'g> Work<'g> {
    fn with(base: &'g SessionGraph, ov: Overlay, fp: Footprint, track: bool) -> Self {
        Self {
            base,
            ov,
            fp: std::cell::RefCell::new(fp),
            track,
        }
    }

    fn note(&self, f: impl FnOnce(&mut Footprint)) {
        if self.track {
            f(&mut self.fp.borrow_mut());
        }
    }

    fn node(&self, id: &str) -> Option<&Node> {
        self.note(|fp| {
            if !fp.nodes.contains(id) {
                fp.nodes.insert(id.to_string());
            }
        });
        self.ov.nodes.get(id).or_else(|| self.base.node(id))
    }

    fn edge(&self, id: &str) -> Option<&Edge> {
        self.note(|fp| {
            if !fp.edges.contains(id) {
                fp.edges.insert(id.to_string());
            }
        });
        self.ov.edges.get(id).or_else(|| self.base.edge(id))
    }

    fn active_node(&self, id: &str) -> bool {
        self.node(id).is_some_and(|n| n.active)
    }

    fn scan(&self) -> impl Iterator<Item = &Node> + '_ {
        self.base
            .nodes
            .iter()
            .map(move |n| self.ov.nodes.get(&n.id).unwrap_or(n))
            .chain(
                self.ov
                    .new_nodes
                    .iter()
                    .filter_map(move |id| self.ov.nodes.get(id)),
            )
    }

    /// Active edges whose own image is active (endpoints not checked).
    fn incident_raw(&self, id: &str, out: bool) -> Vec<&Edge> {
        self.note(|fp| {
            let set = if out { &mut fp.out_of } else { &mut fp.in_of };
            if !set.contains(id) {
                set.insert(id.to_string());
            }
        });
        let (base, extra) = if out {
            (self.base.out_edges(id), self.ov.extra_out.get(id))
        } else {
            (self.base.in_edges(id), self.ov.extra_in.get(id))
        };
        let mut v: Vec<&Edge> = base
            .into_iter()
            .map(|e| self.ov.edges.get(&e.id).unwrap_or(e))
            .collect();
        if let Some(extra) = extra {
            v.extend(extra.iter().filter_map(|eid| self.ov.edges.get(eid)));
        }
        v.retain(|e| e.active);
        v
    }

    /// Traversable edges: active, with both endpoints active.
    fn incident(&self, id: &str, out: bool) -> Vec<&Edge> {
        let mut v = self.incident_raw(id, out);
        v.retain(|e| self.active_node(&e.src) && self.active_node(&e.dst));
        v
    }

    fn in_base_csr(&self, id: &str) -> bool {
        self.base.edge(id).is_some_and(|e| {
            e.active
                && self.base.node(&e.src).is_some_and(|n| n.active)
                && self.base.node(&e.dst).is_some_and(|n| n.active)
        })
    }

    fn put_node(&mut self, node: Node) {
        self.note(|fp| {
            fp.nodes.insert(node.id.clone());
        });
        if self.ov.dirty_node_set.insert(node.id.clone()) {
            self.ov.dirty_nodes.push(node.id.clone());
        }
        if self.base.node(&node.id).is_none() && !self.ov.nodes.contains_key(&node.id) {
            self.ov.new_nodes.push(node.id.clone());
        }
        self.ov.nodes.insert(node.id.clone(), node);
    }

    fn put_edge(&mut self, edge: Edge) {
        self.note(|fp| {
            fp.edges.insert(edge.id.clone());
            fp.out_of.insert(edge.src.clone());
            fp.in_of.insert(edge.dst.clone());
        });
        if self.ov.dirty_edge_set.insert(edge.id.clone()) {
            self.ov.dirty_edges.push(edge.id.clone());
        }
        if !self.in_base_csr(&edge.id) && !self.ov.edges.contains_key(&edge.id) {
            self.ov
                .extra_out
                .entry(edge.src.clone())
                .or_default()
                .push(edge.id.clone());
            self.ov
                .extra_in
                .entry(edge.dst.clone())
                .or_default()
                .push(edge.id.clone());
        }
        self.ov.edges.insert(edge.id.clone(), edge);
    }

    fn changes(&self) -> Changes {
        let mut changes = Changes::default();
        for id in &self.ov.dirty_nodes {
            let node = &self.ov.nodes[id];
            let base_active = self.base.node(id).is_some_and(|n| n.active);
            if node.active {
                let unchanged = self.base.node(id).is_some_and(|b| {
                    b.active && b.type_name == node.type_name && b.props == node.props
                });
                if !unchanged {
                    changes.nodes.push(NodeInput {
                        id: node.id.clone(),
                        type_name: node.type_name.clone(),
                        props: node.props.clone(),
                        origin: node.origin.clone(),
                        first_seen: None,
                        last_seen: None,
                    });
                }
            } else if base_active {
                changes.deactivate.push(id.clone());
            }
        }
        for id in &self.ov.dirty_edges {
            let edge = &self.ov.edges[id];
            let base_active = self.base.edge(id).is_some_and(|e| e.active);
            if edge.active {
                let unchanged = self
                    .base
                    .edge(id)
                    .is_some_and(|b| b.active && b.props == edge.props);
                if !unchanged {
                    changes.edges.push(EdgeInput {
                        id: Some(edge.id.clone()),
                        src: edge.src.clone(),
                        dst: edge.dst.clone(),
                        type_name: edge.type_name.clone(),
                        props: edge.props.clone(),
                        origin: edge.origin.clone(),
                        first_seen: None,
                        last_seen: None,
                    });
                }
            } else if base_active {
                changes.deactivate.push(id.clone());
            }
        }
        changes
    }
}

// ---------------------------------------------------------------------------

struct Exec<'g, 'p> {
    w: Work<'g>,
    params: &'p BTreeMap<String, Val>,
    opts: &'p ExecOptions,
    stats: Stats,
    budget: Cell<usize>,
    rand: RandomState,
    seq: u64,
    /// Counter feeding `rand()`.
    rng: Cell<u64>,
    pending_delete_checks: Vec<String>,
}

type Matches = Vec<(Row, Vec<String>)>;

impl Exec<'_, '_> {
    fn check_rows(&self, n: usize) -> Result<(), CypherError> {
        if n > self.opts.max_rows {
            return Err(CypherError::limit(format!(
                "query produced more than {} rows; add LIMIT or narrow the pattern",
                self.opts.max_rows
            )));
        }
        Ok(())
    }

    fn spend(&self) -> Result<(), CypherError> {
        let left = self.budget.get();
        if left == 0 {
            return Err(CypherError::limit(
                "pattern expansion budget exhausted; bound the variable-length range or add labels/properties",
            ));
        }
        self.budget.set(left - 1);
        Ok(())
    }

    fn lookup(&self, var: &str, row: &Row) -> Result<Val, CypherError> {
        row.get(var)
            .cloned()
            .ok_or_else(|| CypherError::semantic(format!("variable `{var}` not defined")))
    }

    // ---- MATCH ----

    fn match_clause(
        &self,
        rows: Vec<Row>,
        optional: bool,
        patterns: &[PatternPart],
        where_: Option<&Expr>,
    ) -> Result<Vec<Row>, CypherError> {
        let mut out = Vec::new();
        for row in rows {
            let mut found = Vec::new();
            self.match_parts(patterns, 0, row.clone(), Vec::new(), &mut found)?;
            let mut kept = Vec::new();
            for r in found {
                let keep = match where_ {
                    Some(w) => self.eval(w, &r, None)?.truthy()? == Some(true),
                    None => true,
                };
                if keep {
                    kept.push(r);
                }
            }
            if kept.is_empty() && optional {
                let mut r = row;
                for part in patterns {
                    for var in pattern_vars(part) {
                        r.entry(var).or_insert(Val::Null);
                    }
                }
                out.push(r);
            } else {
                out.extend(kept);
            }
            self.check_rows(out.len())?;
        }
        Ok(out)
    }

    fn match_parts(
        &self,
        parts: &[PatternPart],
        i: usize,
        row: Row,
        used: Vec<String>,
        out: &mut Vec<Row>,
    ) -> Result<(), CypherError> {
        if i == parts.len() {
            out.push(row);
            return self.check_rows(out.len());
        }
        for (r, u) in self.match_part(&parts[i], &row, &used)? {
            self.match_parts(parts, i + 1, r, u, out)?;
        }
        Ok(())
    }

    fn match_part(
        &self,
        part: &PatternPart,
        row: &Row,
        used: &[String],
    ) -> Result<Matches, CypherError> {
        if part.shortest.is_some() {
            return self.shortest(part, row, used);
        }
        let props = self.eval_props(&part.start.props, row)?;
        let mut res = Vec::new();
        for id in self.node_candidates(&part.start, props.as_ref(), row) {
            let mut r = row.clone();
            if let Some(v) = &part.start.var {
                r.insert(v.clone(), Val::Node(id.clone()));
            }
            self.expand(
                part,
                0,
                &id,
                r,
                used.to_vec(),
                vec![id.clone()],
                Vec::new(),
                &mut res,
            )?;
        }
        Ok(res)
    }

    fn eval_props(&self, props: &Option<Expr>, row: &Row) -> Result<Option<PropMap>, CypherError> {
        let Some(e) = props else { return Ok(None) };
        match self.eval(e, row, None)? {
            Val::Map(m) => Ok(Some(m)),
            other => Err(CypherError::type_error(format!(
                "a pattern property map must be a MAP, got {}",
                other.type_name()
            ))),
        }
    }

    fn props_match(
        &self,
        id: &str,
        props: &BTreeMap<String, PropValue>,
        want: Option<&PropMap>,
    ) -> bool {
        let Some(want) = want else { return true };
        want.iter().all(|(k, v)| {
            if k == "id" {
                equals(&Val::Str(id.to_string()), v) == Some(true)
            } else {
                props
                    .get(k)
                    .is_some_and(|p| equals(&Val::from_prop(p), v) == Some(true))
            }
        })
    }

    fn node_ok(&self, np: &NodePat, node: &Node, props: Option<&PropMap>, row: &Row) -> bool {
        if !self.opts.visibility.matches(node.active) {
            return false;
        }
        if !np.labels.iter().all(|l| l == &node.type_name) {
            return false;
        }
        if let Some(var) = &np.var {
            if let Some(bound) = row.get(var) {
                if bound != &Val::Node(node.id.clone()) {
                    return false;
                }
            }
        }
        self.props_match(&node.id, &node.props, props)
    }

    fn node_candidates(&self, np: &NodePat, props: Option<&PropMap>, row: &Row) -> Vec<String> {
        if let Some(var) = &np.var {
            if let Some(bound) = row.get(var) {
                return match bound {
                    Val::Node(id) => self
                        .w
                        .node(id)
                        .filter(|n| self.node_ok(np, n, props, row))
                        .map(|n| vec![n.id.clone()])
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
            }
        }
        if let Some(id) = props.and_then(|p| p.get("id")) {
            return match id {
                Val::Str(id) => self
                    .w
                    .node(id)
                    .filter(|n| self.node_ok(np, n, props, row))
                    .map(|n| vec![n.id.clone()])
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
        }
        let ids: Vec<String> = self
            .w
            .scan()
            .filter(|n| self.node_ok(np, n, props, row))
            .map(|n| n.id.clone())
            .collect();
        self.w.note(|fp| {
            if np.labels.is_empty() {
                fp.all_nodes = true;
            } else {
                fp.labels.extend(np.labels.iter().cloned());
            }
            fp.nodes.extend(ids.iter().cloned());
        });
        ids
    }

    /// (edge id, other end) for each traversable edge matching `rp` at `cur`.
    fn incident(&self, cur: &str, rp: &RelPat, props: Option<&PropMap>) -> Vec<(String, String)> {
        let mut v = Vec::new();
        let ok = |e: &Edge| {
            (rp.types.is_empty() || rp.types.iter().any(|t| t == &e.type_name))
                && self.props_match(&e.id, &e.props, props)
        };
        if matches!(rp.dir, Dir::Out | Dir::Both) {
            for e in self.w.incident(cur, true) {
                if ok(e) {
                    v.push((e.id.clone(), e.dst.clone()));
                }
            }
        }
        if matches!(rp.dir, Dir::In | Dir::Both) {
            for e in self.w.incident(cur, false) {
                if ok(e) && !(rp.dir == Dir::Both && e.src == e.dst) {
                    v.push((e.id.clone(), e.src.clone()));
                }
            }
        }
        v
    }

    fn hop_bounds(&self, rp: &RelPat) -> Result<(u32, u32), CypherError> {
        let (min, max) = rp.range.unwrap_or((1, Some(1)));
        let max = match max {
            Some(m) if m > self.opts.max_hops => {
                return Err(CypherError::limit(format!(
                    "variable-length upper bound {m} exceeds the limit of {} hops",
                    self.opts.max_hops
                )))
            }
            Some(m) => m,
            None => self.opts.max_hops,
        };
        Ok((min, max))
    }

    #[allow(clippy::too_many_arguments)]
    fn expand(
        &self,
        part: &PatternPart,
        step: usize,
        cur: &str,
        row: Row,
        used: Vec<String>,
        pnodes: Vec<String>,
        prels: Vec<String>,
        out: &mut Matches,
    ) -> Result<(), CypherError> {
        self.spend()?;
        if step == part.chain.len() {
            let mut row = row;
            if let Some(pv) = &part.path_var {
                row.insert(
                    pv.clone(),
                    Val::Path {
                        nodes: pnodes,
                        rels: prels,
                    },
                );
            }
            out.push((row, used));
            return self.check_rows(out.len());
        }
        let (rp, np) = &part.chain[step];
        let rprops = self.eval_props(&rp.props, &row)?;
        let nprops = self.eval_props(&np.props, &row)?;
        if rp.range.is_none() {
            for (eid, other) in self.incident(cur, rp, rprops.as_ref()) {
                if used.contains(&eid) {
                    continue;
                }
                if let Some(v) = &rp.var {
                    if let Some(b) = row.get(v) {
                        if b != &Val::Rel(eid.clone()) {
                            continue;
                        }
                    }
                }
                let Some(node) = self.w.node(&other) else {
                    continue;
                };
                if !self.node_ok(np, node, nprops.as_ref(), &row) {
                    continue;
                }
                let mut r = row.clone();
                if let Some(v) = &rp.var {
                    r.insert(v.clone(), Val::Rel(eid.clone()));
                }
                if let Some(v) = &np.var {
                    r.insert(v.clone(), Val::Node(other.clone()));
                }
                let mut u = used.clone();
                u.push(eid.clone());
                let mut pn = pnodes.clone();
                pn.push(other.clone());
                let mut pr = prels.clone();
                pr.push(eid);
                self.expand(part, step + 1, &other, r, u, pn, pr, out)?;
            }
            return Ok(());
        }
        let (min, max) = self.hop_bounds(rp)?;
        let mut trail_edges = Vec::new();
        let mut trail_nodes = Vec::new();
        self.var_length(
            part,
            step,
            cur,
            0,
            (min, max),
            rprops.as_ref(),
            nprops.as_ref(),
            &row,
            &used,
            &pnodes,
            &prels,
            &mut trail_edges,
            &mut trail_nodes,
            out,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn var_length(
        &self,
        part: &PatternPart,
        step: usize,
        cur: &str,
        depth: u32,
        bounds: (u32, u32),
        rprops: Option<&PropMap>,
        nprops: Option<&PropMap>,
        row: &Row,
        used: &[String],
        pnodes: &[String],
        prels: &[String],
        trail_edges: &mut Vec<String>,
        trail_nodes: &mut Vec<String>,
        out: &mut Matches,
    ) -> Result<(), CypherError> {
        self.spend()?;
        let (rp, np) = &part.chain[step];
        if depth >= bounds.0 {
            let rel_list = Val::List(trail_edges.iter().cloned().map(Val::Rel).collect());
            let rel_ok = match rp.var.as_ref().and_then(|v| row.get(v)) {
                Some(b) => equals(b, &rel_list) == Some(true),
                None => true,
            };
            if rel_ok {
                if let Some(node) = self.w.node(cur) {
                    if self.node_ok(np, node, nprops, row) {
                        let mut r = row.clone();
                        if let Some(v) = &rp.var {
                            r.insert(v.clone(), rel_list);
                        }
                        if let Some(v) = &np.var {
                            r.insert(v.clone(), Val::Node(cur.to_string()));
                        }
                        let mut u = used.to_vec();
                        u.extend(trail_edges.iter().cloned());
                        let mut pn = pnodes.to_vec();
                        pn.extend(trail_nodes.iter().cloned());
                        let mut pr = prels.to_vec();
                        pr.extend(trail_edges.iter().cloned());
                        self.expand(part, step + 1, cur, r, u, pn, pr, out)?;
                    }
                }
            }
        }
        if depth >= bounds.1 {
            return Ok(());
        }
        for (eid, other) in self.incident(cur, rp, rprops) {
            if used.contains(&eid) || trail_edges.contains(&eid) {
                continue;
            }
            trail_edges.push(eid);
            trail_nodes.push(other.clone());
            self.var_length(
                part,
                step,
                &other,
                depth + 1,
                bounds,
                rprops,
                nprops,
                row,
                used,
                pnodes,
                prels,
                trail_edges,
                trail_nodes,
                out,
            )?;
            trail_edges.pop();
            trail_nodes.pop();
        }
        Ok(())
    }

    fn shortest(
        &self,
        part: &PatternPart,
        row: &Row,
        used: &[String],
    ) -> Result<Matches, CypherError> {
        let all = part.shortest == Some(true);
        let (rp, np) = &part.chain[0];
        let (min, max) = self.hop_bounds(rp)?;
        if min > 1 {
            return Err(CypherError::semantic(
                "shortestPath supports a minimum length of 0 or 1",
            ));
        }
        let sprops = self.eval_props(&part.start.props, row)?;
        let eprops = self.eval_props(&np.props, row)?;
        let rprops = self.eval_props(&rp.props, row)?;
        let starts = self.node_candidates(&part.start, sprops.as_ref(), row);
        let mut res = Vec::new();
        for s in &starts {
            let mut r0 = row.clone();
            if let Some(v) = &part.start.var {
                r0.insert(v.clone(), Val::Node(s.clone()));
            }
            let ends = self.node_candidates(np, eprops.as_ref(), &r0);
            for t in ends {
                // BFS distance.
                let mut dist: HashMap<String, u32> = HashMap::from([(s.clone(), 0)]);
                let mut queue = VecDeque::from([s.clone()]);
                while let Some(n) = queue.pop_front() {
                    self.spend()?;
                    if n == t {
                        break;
                    }
                    let d = dist[&n];
                    if d >= max {
                        continue;
                    }
                    for (eid, other) in self.incident(&n, rp, rprops.as_ref()) {
                        if used.contains(&eid) {
                            continue;
                        }
                        if !dist.contains_key(&other) {
                            dist.insert(other.clone(), d + 1);
                            queue.push_back(other);
                        }
                    }
                }
                let Some(&d) = dist.get(&t) else { continue };
                if d < min {
                    continue;
                }
                let mut paths = Vec::new();
                let mut trail = (vec![s.clone()], Vec::new());
                self.paths_of_len(
                    s,
                    &t,
                    d,
                    rp,
                    rprops.as_ref(),
                    used,
                    &mut trail,
                    &mut paths,
                    all,
                )?;
                for (pn, pr) in paths {
                    let mut r = r0.clone();
                    if let Some(v) = &np.var {
                        r.insert(v.clone(), Val::Node(t.clone()));
                    }
                    if let Some(v) = &rp.var {
                        r.insert(
                            v.clone(),
                            Val::List(pr.iter().cloned().map(Val::Rel).collect()),
                        );
                    }
                    if let Some(pv) = &part.path_var {
                        r.insert(
                            pv.clone(),
                            Val::Path {
                                nodes: pn,
                                rels: pr.clone(),
                            },
                        );
                    }
                    let mut u = used.to_vec();
                    u.extend(pr);
                    res.push((r, u));
                    self.check_rows(res.len())?;
                }
            }
        }
        Ok(res)
    }

    #[allow(clippy::too_many_arguments)]
    fn paths_of_len(
        &self,
        cur: &str,
        target: &str,
        left: u32,
        rp: &RelPat,
        rprops: Option<&PropMap>,
        used: &[String],
        trail: &mut (Vec<String>, Vec<String>),
        out: &mut Vec<(Vec<String>, Vec<String>)>,
        all: bool,
    ) -> Result<(), CypherError> {
        self.spend()?;
        if !all && !out.is_empty() {
            return Ok(());
        }
        if left == 0 {
            if cur == target {
                out.push(trail.clone());
            }
            return Ok(());
        }
        for (eid, other) in self.incident(cur, rp, rprops) {
            if used.contains(&eid) || trail.1.contains(&eid) || trail.0.contains(&other) {
                continue;
            }
            trail.0.push(other.clone());
            trail.1.push(eid);
            self.paths_of_len(&other, target, left - 1, rp, rprops, used, trail, out, all)?;
            trail.0.pop();
            trail.1.pop();
        }
        Ok(())
    }

    // ---- projection ----

    fn project(
        &self,
        rows: Vec<Row>,
        p: &Projection,
    ) -> Result<(Vec<String>, Vec<Row>), CypherError> {
        let mut items: Vec<(Expr, String)> = Vec::new();
        if p.star {
            if let Some(first) = rows.first() {
                for name in first.keys() {
                    if !p.items.iter().any(|(_, a)| a == name) {
                        items.push((Expr::Var(name.clone()), name.clone()));
                    }
                }
            }
        }
        items.extend(p.items.iter().cloned());
        let mut seen = HashSet::new();
        for (_, alias) in &items {
            if !seen.insert(alias.clone()) {
                return Err(CypherError::semantic(format!(
                    "multiple result columns named `{alias}`"
                )));
            }
        }
        let columns: Vec<String> = items.iter().map(|(_, a)| a.clone()).collect();
        let aggregating = items.iter().any(|(e, _)| e.contains_aggregate());

        // (projected row, sort keys, environment for WITH ... WHERE)
        let mut out: Vec<(Row, Vec<Val>, Row)> = Vec::new();
        if !aggregating {
            for row in &rows {
                let mut nr = Row::new();
                for (e, a) in &items {
                    nr.insert(a.clone(), self.eval(e, row, None)?);
                }
                let mut env = row.clone();
                env.extend(nr.iter().map(|(k, v)| (k.clone(), v.clone())));
                let mut keys = Vec::with_capacity(p.order.len());
                for (e, _) in &p.order {
                    keys.push(self.eval(e, &env, None)?);
                }
                out.push((nr, keys, env));
            }
        } else {
            let mut aggs = Vec::new();
            let plan: Vec<(Expr, String, bool)> = items
                .iter()
                .map(|(e, a)| {
                    if e.contains_aggregate() {
                        (rewrite_aggs(e, &mut aggs), a.clone(), false)
                    } else {
                        (e.clone(), a.clone(), true)
                    }
                })
                .collect();
            let order_plan: Vec<Expr> = p
                .order
                .iter()
                .map(|(e, _)| rewrite_aggs(e, &mut aggs))
                .collect();
            let mut index: HashMap<String, usize> = HashMap::new();
            let mut groups: Vec<(Vec<Val>, Vec<&Row>)> = Vec::new();
            for row in &rows {
                let mut kv = Vec::new();
                for (e, _, is_key) in &plan {
                    if *is_key {
                        kv.push(self.eval(e, row, None)?);
                    }
                }
                let k = key(&Val::List(kv.clone()));
                match index.get(&k) {
                    Some(&g) => groups[g].1.push(row),
                    None => {
                        index.insert(k, groups.len());
                        groups.push((kv, vec![row]));
                    }
                }
            }
            let empty = Row::new();
            if groups.is_empty() && plan.iter().all(|(_, _, is_key)| !is_key) {
                groups.push((Vec::new(), Vec::new()));
            }
            for (kv, members) in &groups {
                let mut vals = Vec::with_capacity(aggs.len());
                for agg in &aggs {
                    vals.push(self.aggregate(agg, members)?);
                }
                let first = members.first().copied().unwrap_or(&empty);
                let mut nr = Row::new();
                let mut ki = 0;
                for (e, a, is_key) in &plan {
                    let v = if *is_key {
                        ki += 1;
                        kv[ki - 1].clone()
                    } else {
                        self.eval(e, first, Some(&vals))?
                    };
                    nr.insert(a.clone(), v);
                }
                let mut env = first.clone();
                env.extend(nr.iter().map(|(k, v)| (k.clone(), v.clone())));
                let mut keys = Vec::with_capacity(order_plan.len());
                for e in &order_plan {
                    keys.push(self.eval(e, &env, Some(&vals))?);
                }
                out.push((nr, keys, env));
            }
        }
        if p.distinct {
            let mut seen = HashSet::new();
            out.retain(|(r, _, _)| {
                let k = key(&Val::List(columns.iter().map(|c| r[c].clone()).collect()));
                seen.insert(k)
            });
        }
        if !p.order.is_empty() {
            let dirs: Vec<bool> = p.order.iter().map(|(_, asc)| *asc).collect();
            out.sort_by(|(_, a, _), (_, b, _)| {
                for (i, asc) in dirs.iter().enumerate() {
                    let o = order(&a[i], &b[i]);
                    let o = if *asc { o } else { o.reverse() };
                    if o != std::cmp::Ordering::Equal {
                        return o;
                    }
                }
                std::cmp::Ordering::Equal
            });
        }
        let empty = Row::new();
        let skip = match &p.skip {
            Some(e) => self.non_negative(e, &empty, "SKIP")?,
            None => 0,
        };
        let limit = match &p.limit {
            Some(e) => Some(self.non_negative(e, &empty, "LIMIT")?),
            None => None,
        };
        // `WITH ... WHERE` sees the projected variables and, as in openCypher, the
        // variables in scope before the projection.
        let mut rows_out: Vec<Row> = Vec::new();
        for (r, _, env) in out.into_iter().skip(skip).take(limit.unwrap_or(usize::MAX)) {
            if let Some(w) = &p.where_ {
                if self.eval(w, &env, None)?.truthy()? != Some(true) {
                    continue;
                }
            }
            rows_out.push(r);
        }
        Ok((columns, rows_out))
    }

    fn non_negative(&self, e: &Expr, row: &Row, what: &str) -> Result<usize, CypherError> {
        match self.eval(e, row, None)? {
            Val::Int(i) if i >= 0 => Ok(i as usize),
            other => Err(CypherError::semantic(format!(
                "{what} needs a non-negative integer, got {}",
                other.type_name()
            ))),
        }
    }

    fn aggregate(&self, agg: &Expr, rows: &[&Row]) -> Result<Val, CypherError> {
        let (name, distinct, args) = match agg {
            Expr::CountStar => return Ok(Val::Int(rows.len() as i64)),
            Expr::Func {
                name,
                distinct,
                args,
            } => (name.as_str(), *distinct, args),
            _ => unreachable!("only aggregates are planned"),
        };
        if args.len() != 1 {
            return Err(CypherError::semantic(format!(
                "{name}() takes one argument"
            )));
        }
        let mut vals = Vec::new();
        let mut seen = HashSet::new();
        for row in rows {
            let v = self.eval(&args[0], row, None)?;
            if v.is_null() {
                continue;
            }
            if distinct && !seen.insert(key(&v)) {
                continue;
            }
            vals.push(v);
        }
        Ok(match name {
            "count" => Val::Int(vals.len() as i64),
            "collect" => Val::List(vals),
            "sum" => {
                let mut int_sum: i64 = 0;
                let mut float_sum = 0.0;
                let mut is_float = false;
                for v in &vals {
                    match v {
                        Val::Int(i) => {
                            int_sum = int_sum.checked_add(*i).ok_or_else(|| {
                                CypherError::type_error("integer overflow in sum()")
                            })?;
                            float_sum += *i as f64;
                        }
                        Val::Float(f) => {
                            is_float = true;
                            float_sum += f;
                        }
                        other => {
                            return Err(CypherError::type_error(format!(
                                "sum() of {}",
                                other.type_name()
                            )))
                        }
                    }
                }
                if is_float {
                    Val::Float(float_sum)
                } else {
                    Val::Int(int_sum)
                }
            }
            "avg" => {
                if vals.is_empty() {
                    Val::Null
                } else {
                    let mut total = 0.0;
                    for v in &vals {
                        total += match v {
                            Val::Int(i) => *i as f64,
                            Val::Float(f) => *f,
                            other => {
                                return Err(CypherError::type_error(format!(
                                    "avg() of {}",
                                    other.type_name()
                                )))
                            }
                        };
                    }
                    Val::Float(total / vals.len() as f64)
                }
            }
            "min" => vals.into_iter().min_by(order).unwrap_or(Val::Null),
            "max" => vals.into_iter().max_by(order).unwrap_or(Val::Null),
            _ => unreachable!(),
        })
    }

    // ---- writes ----

    fn next_id(&mut self, label: &str) -> String {
        self.seq += 1;
        let mut h = self.rand.build_hasher();
        h.write_u64(self.seq);
        h.write_i64(self.opts.now_ms);
        format!("{}:{:016x}", label.to_ascii_lowercase(), h.finish())
    }

    fn create_part(
        &mut self,
        part: &PatternPart,
        row: Row,
        merge: bool,
    ) -> Result<Row, CypherError> {
        if part.shortest.is_some() {
            return Err(CypherError::semantic("shortestPath cannot be created"));
        }
        let mut row = row;
        let first = self.create_or_bind(&part.start, &mut row)?;
        let mut prev = first.clone();
        let mut pnodes = vec![first];
        let mut prels = Vec::new();
        for (rp, np) in &part.chain {
            let next = self.create_or_bind(np, &mut row)?;
            if rp.range.is_some() {
                return Err(CypherError::semantic(
                    "variable-length relationships cannot be created",
                ));
            }
            if rp.types.len() != 1 {
                return Err(CypherError::semantic(
                    "a created relationship needs exactly one type",
                ));
            }
            if rp.dir == Dir::Both && !merge {
                return Err(CypherError::semantic(
                    "only directed relationships can be created",
                ));
            }
            let (src, dst) = if rp.dir == Dir::In {
                (next.clone(), prev.clone())
            } else {
                (prev.clone(), next.clone())
            };
            let eid = self.create_edge(rp, &src, &dst, &row)?;
            if let Some(v) = &rp.var {
                if row.contains_key(v) {
                    return Err(CypherError::semantic(format!(
                        "variable `{v}` already declared"
                    )));
                }
                row.insert(v.clone(), Val::Rel(eid.clone()));
            }
            prels.push(eid);
            pnodes.push(next.clone());
            prev = next;
        }
        if let Some(pv) = &part.path_var {
            row.insert(
                pv.clone(),
                Val::Path {
                    nodes: pnodes,
                    rels: prels,
                },
            );
        }
        Ok(row)
    }

    fn create_or_bind(&mut self, np: &NodePat, row: &mut Row) -> Result<String, CypherError> {
        if let Some(var) = &np.var {
            if let Some(bound) = row.get(var) {
                return match bound {
                    Val::Node(id) => {
                        if !np.labels.is_empty() || np.props.is_some() {
                            return Err(CypherError::semantic(format!(
                                "variable `{var}` is already bound; it cannot be redeclared with labels or properties"
                            )));
                        }
                        Ok(id.clone())
                    }
                    Val::Null => Err(CypherError::semantic(format!(
                        "cannot create a relationship to null variable `{var}`"
                    ))),
                    other => Err(CypherError::type_error(format!(
                        "`{var}` is a {}, not a node",
                        other.type_name()
                    ))),
                };
            }
        }
        let id = self.create_node(np, row)?;
        if let Some(var) = &np.var {
            row.insert(var.clone(), Val::Node(id.clone()));
        }
        Ok(id)
    }

    fn element_id(
        &mut self,
        props: &mut PropMap,
        label: &str,
        node: bool,
    ) -> Result<Option<String>, CypherError> {
        let id = match props.remove("id") {
            None | Some(Val::Null) => {
                if node {
                    Some(self.next_id(label))
                } else {
                    None
                }
            }
            Some(Val::Str(s)) => Some(s),
            Some(Val::Int(i)) => Some(i.to_string()),
            Some(other) => {
                return Err(CypherError::type_error(format!(
                    "id must be a STRING, got {}",
                    other.type_name()
                )))
            }
        };
        if let Some(id) = &id {
            validate_id(id).map_err(|e| CypherError::semantic(e.message))?;
        }
        Ok(id)
    }

    fn to_props(props: &PropMap) -> Result<BTreeMap<String, PropValue>, CypherError> {
        let mut out = BTreeMap::new();
        for (k, v) in props {
            if let Some(p) = v.to_prop()? {
                out.insert(k.clone(), p);
            }
        }
        Ok(out)
    }

    fn create_node(&mut self, np: &NodePat, row: &Row) -> Result<String, CypherError> {
        if np.labels.len() != 1 {
            return Err(CypherError::semantic(format!(
                "ehgraph nodes carry exactly one label (the node type); got {}",
                np.labels.len()
            )));
        }
        let label = np.labels[0].clone();
        if label.is_empty() || label.len() > 128 {
            return Err(CypherError::semantic("a label must be 1..=128 characters"));
        }
        let mut props = self.eval_props(&np.props, row)?.unwrap_or_default();
        let id = self
            .element_id(&mut props, &label, true)?
            .expect("node ids are generated");
        let existing = self.w.node(&id).cloned();
        if existing.as_ref().is_some_and(|n| n.active) {
            return Err(CypherError::constraint(format!(
                "node `{id}` already exists; use MERGE to match-or-create by id"
            )));
        }
        if self.w.edge(&id).is_some() {
            return Err(CypherError::constraint(format!(
                "`{id}` is already a relationship id"
            )));
        }
        let props = Self::to_props(&props)?;
        self.stats.nodes_created += 1;
        self.stats.properties_set += props.len() as u64;
        let now = self.opts.now_ms;
        self.w.put_node(Node {
            id: id.clone(),
            type_name: label,
            props,
            first_seen: existing.as_ref().map(|n| n.first_seen).unwrap_or(now),
            last_seen: now,
            active: true,
            origin: existing.as_ref().and_then(|n| n.origin.clone()),
            source: self.opts.source.clone(),
            execution_id: self.opts.execution_id.clone(),
            version_created: existing.as_ref().map(|n| n.version_created).unwrap_or(0),
            version_updated: existing.as_ref().map(|n| n.version_updated).unwrap_or(0),
        });
        Ok(id)
    }

    fn create_edge(
        &mut self,
        rp: &RelPat,
        src: &str,
        dst: &str,
        row: &Row,
    ) -> Result<String, CypherError> {
        let type_name = rp.types[0].clone();
        let mut props = self.eval_props(&rp.props, row)?.unwrap_or_default();
        let id = self
            .element_id(&mut props, &type_name, false)?
            .unwrap_or_else(|| default_edge_id(src, &type_name, dst, None));
        let existing = self.w.edge(&id).cloned();
        if existing.as_ref().is_some_and(|e| e.active) {
            return Err(CypherError::constraint(format!(
                "relationship `{id}` already exists; ehgraph keeps one edge per (start, type, end) unless an explicit id is given — use MERGE"
            )));
        }
        if self.w.node(&id).is_some() {
            return Err(CypherError::constraint(format!(
                "`{id}` is already a node id"
            )));
        }
        if let Some(e) = &existing {
            if e.src != src || e.dst != dst {
                return Err(CypherError::constraint(format!(
                    "relationship id `{id}` belongs to different endpoints"
                )));
            }
        }
        let props = Self::to_props(&props)?;
        self.stats.relationships_created += 1;
        self.stats.properties_set += props.len() as u64;
        let now = self.opts.now_ms;
        self.w.put_edge(Edge {
            id: id.clone(),
            src: src.to_string(),
            dst: dst.to_string(),
            type_name,
            props,
            first_seen: existing.as_ref().map(|e| e.first_seen).unwrap_or(now),
            last_seen: now,
            active: true,
            origin: existing.as_ref().and_then(|e| e.origin.clone()),
            source: self.opts.source.clone(),
            execution_id: self.opts.execution_id.clone(),
            version_created: existing.as_ref().map(|e| e.version_created).unwrap_or(0),
            version_updated: existing.as_ref().map(|e| e.version_updated).unwrap_or(0),
        });
        Ok(id)
    }

    fn apply_set(&mut self, items: &[SetItem], row: &mut Row) -> Result<(), CypherError> {
        for item in items {
            match item {
                SetItem::Prop { var, key, value } => {
                    let target = self.lookup(var, row)?;
                    let v = self.eval(value, row, None)?;
                    self.set_prop(&target, key, v)?;
                }
                SetItem::Replace { var, value } | SetItem::Merge { var, value } => {
                    let replace = matches!(item, SetItem::Replace { .. });
                    let target = self.lookup(var, row)?;
                    let map = match self.eval(value, row, None)? {
                        Val::Map(m) => m,
                        v @ (Val::Node(_) | Val::Rel(_)) => self.properties_of(&v),
                        Val::Null => PropMap::new(),
                        other => {
                            return Err(CypherError::type_error(format!(
                                "SET {var} {} expects a MAP, got {}",
                                if replace { "=" } else { "+=" },
                                other.type_name()
                            )))
                        }
                    };
                    self.update_props(&target, |eid, props| {
                        let mut touched = 0;
                        if replace {
                            touched += props.len() as u64;
                            props.clear();
                        }
                        for (k, v) in &map {
                            if k == "id" {
                                if equals(&Val::Str(eid.to_string()), v) == Some(true) {
                                    continue;
                                }
                                return Err(CypherError::semantic(
                                    "`id` is the element id and cannot be changed",
                                ));
                            }
                            match v.to_prop()? {
                                Some(p) => {
                                    props.insert(k.clone(), p);
                                }
                                None => {
                                    props.remove(k);
                                }
                            }
                            touched += 1;
                        }
                        Ok(touched)
                    })?;
                }
                SetItem::Labels { var, labels } => {
                    let target = self.lookup(var, row)?;
                    match target {
                        Val::Null => {}
                        Val::Node(id) => {
                            let current = self
                                .w
                                .node(&id)
                                .map(|n| n.type_name.clone())
                                .unwrap_or_default();
                            if !labels.iter().all(|l| l == &current) {
                                return Err(CypherError::unsupported(
                                    "SET n:Label adding a second label (ehgraph nodes carry exactly one label)",
                                ));
                            }
                        }
                        other => {
                            return Err(CypherError::type_error(format!(
                                "cannot set labels on a {}",
                                other.type_name()
                            )))
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn set_prop(&mut self, target: &Val, key: &str, v: Val) -> Result<(), CypherError> {
        if key == "id" {
            return Err(CypherError::semantic(
                "`id` is the element id and cannot be changed",
            ));
        }
        let p = v.to_prop()?;
        self.update_props(target, |_, props| {
            match p {
                Some(p) => {
                    props.insert(key.to_string(), p);
                }
                None => {
                    props.remove(key);
                }
            }
            Ok(1)
        })
    }

    /// Apply `f` to a copy of the element's properties; record a change only if
    /// the result differs (so re-running a SET is a no-op, like an identical upsert).
    fn update_props(
        &mut self,
        target: &Val,
        f: impl FnOnce(&str, &mut BTreeMap<String, PropValue>) -> Result<u64, CypherError>,
    ) -> Result<(), CypherError> {
        match target {
            Val::Null => Ok(()),
            Val::Node(id) => {
                let Some(node) = self.w.node(id).filter(|n| n.active).cloned() else {
                    return Err(CypherError::semantic(format!(
                        "node `{id}` was deleted in this query"
                    )));
                };
                let mut props = node.props.clone();
                f(id, &mut props)?;
                if props != node.props {
                    self.stats.properties_set += diff_count(&node.props, &props);
                    let mut node = node;
                    node.props = props;
                    self.w.put_node(node);
                }
                Ok(())
            }
            Val::Rel(id) => {
                let Some(edge) = self.w.edge(id).filter(|e| e.active).cloned() else {
                    return Err(CypherError::semantic(format!(
                        "relationship `{id}` was deleted in this query"
                    )));
                };
                let mut props = edge.props.clone();
                f(id, &mut props)?;
                if props != edge.props {
                    self.stats.properties_set += diff_count(&edge.props, &props);
                    let mut edge = edge;
                    edge.props = props;
                    self.w.put_edge(edge);
                }
                Ok(())
            }
            other => Err(CypherError::type_error(format!(
                "cannot set properties on a {}",
                other.type_name()
            ))),
        }
    }

    fn deactivate_edge(&mut self, id: &str) {
        if let Some(edge) = self.w.edge(id).filter(|e| e.active).cloned() {
            let mut edge = edge;
            edge.active = false;
            edge.last_seen = self.opts.now_ms;
            self.w.put_edge(edge);
            self.stats.relationships_deleted += 1;
        }
    }

    fn delete(&mut self, v: Val, detach: bool) -> Result<(), CypherError> {
        match v {
            Val::Null => {}
            Val::Rel(id) => self.deactivate_edge(&id),
            Val::Node(id) => {
                let Some(node) = self.w.node(&id).filter(|n| n.active).cloned() else {
                    return Ok(());
                };
                if detach {
                    let mut ids: Vec<String> = self
                        .w
                        .incident_raw(&id, true)
                        .into_iter()
                        .chain(self.w.incident_raw(&id, false))
                        .map(|e| e.id.clone())
                        .collect();
                    ids.sort();
                    ids.dedup();
                    for eid in ids {
                        self.deactivate_edge(&eid);
                    }
                } else {
                    self.pending_delete_checks.push(id.clone());
                }
                let mut node = node;
                node.active = false;
                node.last_seen = self.opts.now_ms;
                self.w.put_node(node);
                self.stats.nodes_deleted += 1;
            }
            Val::Path { nodes, rels } => {
                for r in rels {
                    self.deactivate_edge(&r);
                }
                for n in nodes {
                    self.delete(Val::Node(n), detach)?;
                }
            }
            Val::List(items) => {
                for item in items {
                    self.delete(item, detach)?;
                }
            }
            other => {
                return Err(CypherError::type_error(format!(
                    "DELETE expects a node, relationship or path, got {}",
                    other.type_name()
                )))
            }
        }
        Ok(())
    }

    /// Like Neo4j, a plain DELETE of a node that still has relationships fails at
    /// the end of the query (so `DELETE r, n` in any order works).
    fn verify_deletes(&self) -> Result<(), CypherError> {
        for id in &self.pending_delete_checks {
            let remaining =
                self.w.incident_raw(id, true).len() + self.w.incident_raw(id, false).len();
            if remaining > 0 {
                return Err(CypherError::constraint(format!(
                    "cannot delete node `{id}` because it still has relationships; delete them first or use DETACH DELETE"
                )));
            }
        }
        Ok(())
    }

    // ---- expressions ----

    fn properties_of(&self, v: &Val) -> PropMap {
        let props = match v {
            Val::Node(id) => self.w.node(id).map(|n| &n.props),
            Val::Rel(id) => self.w.edge(id).map(|e| &e.props),
            _ => None,
        };
        props
            .map(|p| {
                p.iter()
                    .map(|(k, v)| (k.clone(), Val::from_prop(v)))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn prop_of(&self, v: &Val, key: &str) -> Result<Val, CypherError> {
        Ok(match v {
            Val::Null => Val::Null,
            Val::Node(id) => {
                if key == "id" {
                    return Ok(Val::Str(id.clone()));
                }
                self.w
                    .node(id)
                    .and_then(|n| n.props.get(key))
                    .map(Val::from_prop)
                    .unwrap_or(Val::Null)
            }
            Val::Rel(id) => {
                if key == "id" {
                    return Ok(Val::Str(id.clone()));
                }
                self.w
                    .edge(id)
                    .and_then(|e| e.props.get(key))
                    .map(Val::from_prop)
                    .unwrap_or(Val::Null)
            }
            Val::Map(m) => m.get(key).cloned().unwrap_or(Val::Null),
            other => {
                return Err(CypherError::type_error(format!(
                    "cannot read property `{key}` of a {}",
                    other.type_name()
                )))
            }
        })
    }

    fn eval(&self, e: &Expr, row: &Row, aggs: Option<&[Val]>) -> Result<Val, CypherError> {
        Ok(match e {
            Expr::Lit(v) => v.clone(),
            Expr::Param(name) => self
                .params
                .get(name)
                .cloned()
                .ok_or_else(|| CypherError::semantic(format!("missing parameter ${name}")))?,
            Expr::Var(name) => self.lookup(name, row)?,
            Expr::Prop(base, key) => {
                let b = self.eval(base, row, aggs)?;
                self.prop_of(&b, key)?
            }
            Expr::Index(base, idx) => {
                let b = self.eval(base, row, aggs)?;
                let i = self.eval(idx, row, aggs)?;
                match (&b, &i) {
                    (Val::Null, _) | (_, Val::Null) => Val::Null,
                    (Val::List(items), Val::Int(n)) => {
                        let len = items.len() as i64;
                        let n = if *n < 0 { len + n } else { *n };
                        if n < 0 || n >= len {
                            Val::Null
                        } else {
                            items[n as usize].clone()
                        }
                    }
                    (Val::Map(_) | Val::Node(_) | Val::Rel(_), Val::Str(k)) => {
                        self.prop_of(&b, k)?
                    }
                    _ => {
                        return Err(CypherError::type_error(format!(
                            "cannot index a {} with a {}",
                            b.type_name(),
                            i.type_name()
                        )))
                    }
                }
            }
            Expr::Slice(base, lo, hi) => {
                let b = self.eval(base, row, aggs)?;
                let Val::List(items) = b else {
                    return if b.is_null() {
                        Ok(Val::Null)
                    } else {
                        Err(CypherError::type_error("slicing needs a LIST"))
                    };
                };
                let len = items.len() as i64;
                let bound =
                    |x: &Option<Box<Expr>>, default: i64| -> Result<Option<i64>, CypherError> {
                        match x {
                            None => Ok(Some(default)),
                            Some(e) => match self.eval(e, row, aggs)? {
                                Val::Int(n) => {
                                    Ok(Some(if n < 0 { (len + n).max(0) } else { n.min(len) }))
                                }
                                Val::Null => Ok(None),
                                _ => Err(CypherError::type_error("slice bounds must be integers")),
                            },
                        }
                    };
                match (bound(lo, 0)?, bound(hi, len)?) {
                    (Some(a), Some(b)) if a < b => {
                        Val::List(items[a as usize..b as usize].to_vec())
                    }
                    (Some(_), Some(_)) => Val::List(Vec::new()),
                    _ => Val::Null,
                }
            }
            Expr::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.eval(item, row, aggs)?);
                }
                Val::List(out)
            }
            Expr::Map(entries) => {
                let mut out = BTreeMap::new();
                for (k, v) in entries {
                    out.insert(k.clone(), self.eval(v, row, aggs)?);
                }
                Val::Map(out)
            }
            Expr::Unary(UnOp::Not, inner) => match self.eval(inner, row, aggs)?.truthy()? {
                Some(b) => Val::Bool(!b),
                None => Val::Null,
            },
            Expr::Unary(UnOp::Neg, inner) => match self.eval(inner, row, aggs)? {
                Val::Int(i) => Val::Int(
                    i.checked_neg()
                        .ok_or_else(|| CypherError::type_error("integer overflow"))?,
                ),
                Val::Float(f) => Val::Float(-f),
                Val::Null => Val::Null,
                other => {
                    return Err(CypherError::type_error(format!(
                        "cannot negate a {}",
                        other.type_name()
                    )))
                }
            },
            Expr::Binary(op, l, r) => self.binary(*op, l, r, row, aggs)?,
            Expr::IsNull(inner, negated) => {
                let null = self.eval(inner, row, aggs)?.is_null();
                Val::Bool(null != *negated)
            }
            Expr::HasLabels(inner, labels) => match self.eval(inner, row, aggs)? {
                Val::Null => Val::Null,
                Val::Node(id) => {
                    let t = self.w.node(&id).map(|n| n.type_name.as_str()).unwrap_or("");
                    Val::Bool(labels.iter().all(|l| l == t))
                }
                other => {
                    return Err(CypherError::type_error(format!(
                        "label test on a {}",
                        other.type_name()
                    )))
                }
            },
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                let subject = match operand {
                    Some(o) => Some(self.eval(o, row, aggs)?),
                    None => None,
                };
                for (cond, then) in whens {
                    let c = self.eval(cond, row, aggs)?;
                    let hit = match &subject {
                        Some(s) => equals(s, &c) == Some(true),
                        None => c.truthy()? == Some(true),
                    };
                    if hit {
                        return self.eval(then, row, aggs);
                    }
                }
                match else_ {
                    Some(e) => self.eval(e, row, aggs)?,
                    None => Val::Null,
                }
            }
            Expr::ListComp {
                var,
                list,
                pred,
                proj,
            } => {
                let items = match self.eval(list, row, aggs)? {
                    Val::Null => return Ok(Val::Null),
                    Val::List(items) => items,
                    other => {
                        return Err(CypherError::type_error(format!(
                            "list comprehension over a {}",
                            other.type_name()
                        )))
                    }
                };
                let mut out = Vec::new();
                let mut scope = row.clone();
                for item in items {
                    scope.insert(var.clone(), item.clone());
                    if let Some(p) = pred {
                        if self.eval(p, &scope, aggs)?.truthy()? != Some(true) {
                            continue;
                        }
                    }
                    out.push(match proj {
                        Some(e) => self.eval(e, &scope, aggs)?,
                        None => item,
                    });
                }
                Val::List(out)
            }
            Expr::Quantifier {
                kind,
                var,
                list,
                pred,
            } => {
                let items = match self.eval(list, row, aggs)? {
                    Val::Null => return Ok(Val::Null),
                    Val::List(items) => items,
                    other => {
                        return Err(CypherError::type_error(format!(
                            "quantifier over a {}",
                            other.type_name()
                        )))
                    }
                };
                let (mut yes, mut no, mut unknown) = (0usize, 0usize, false);
                let mut scope = row.clone();
                for item in &items {
                    scope.insert(var.clone(), item.clone());
                    match self.eval(pred, &scope, aggs)?.truthy()? {
                        Some(true) => yes += 1,
                        Some(false) => no += 1,
                        None => unknown = true,
                    }
                }
                let decided = match kind {
                    Quant::All => (no > 0).then_some(false).or((!unknown).then_some(true)),
                    Quant::Any => (yes > 0).then_some(true).or((!unknown).then_some(false)),
                    Quant::None => (yes > 0).then_some(false).or((!unknown).then_some(true)),
                    Quant::Single => (yes > 1)
                        .then_some(false)
                        .or((!unknown).then_some(yes == 1)),
                };
                decided.map(Val::Bool).unwrap_or(Val::Null)
            }
            Expr::AggRef(i) => aggs
                .and_then(|a| a.get(*i))
                .cloned()
                .ok_or_else(|| CypherError::semantic("aggregate used outside RETURN or WITH"))?,
            Expr::CountStar => {
                return Err(CypherError::semantic(
                    "count(*) is only allowed in RETURN or WITH",
                ))
            }
            Expr::Exists { patterns, where_ } => {
                let mut found = Vec::new();
                self.match_parts(patterns, 0, row.clone(), Vec::new(), &mut found)?;
                for r in found {
                    let keep = match where_ {
                        Some(w) => self.eval(w, &r, aggs)?.truthy()? == Some(true),
                        None => true,
                    };
                    if keep {
                        return Ok(Val::Bool(true));
                    }
                }
                Val::Bool(false)
            }
            Expr::PatternComp { part, pred, proj } => {
                let mut out = Vec::new();
                for (r, _) in self.match_part(part, row, &[])? {
                    if let Some(p) = pred {
                        if self.eval(p, &r, aggs)?.truthy()? != Some(true) {
                            continue;
                        }
                    }
                    out.push(self.eval(proj, &r, aggs)?);
                    self.check_rows(out.len())?;
                }
                Val::List(out)
            }
            Expr::Func { name, args, .. }
                if name == "exists" && matches!(args.as_slice(), [Expr::Exists { .. }]) =>
            {
                self.eval(&args[0], row, aggs)?
            }
            Expr::Func { name, args, .. } => {
                if AGGREGATES.contains(&name.as_str()) {
                    return Err(CypherError::semantic(format!(
                        "aggregate {name}() is only allowed in RETURN or WITH"
                    )));
                }
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval(a, row, aggs)?);
                }
                self.call(name, vals)?
            }
        })
    }

    fn binary(
        &self,
        op: BinOp,
        l: &Expr,
        r: &Expr,
        row: &Row,
        aggs: Option<&[Val]>,
    ) -> Result<Val, CypherError> {
        if matches!(op, BinOp::And | BinOp::Or | BinOp::Xor) {
            let a = self.eval(l, row, aggs)?.truthy()?;
            let b = self.eval(r, row, aggs)?.truthy()?;
            return Ok(match op {
                BinOp::And => match (a, b) {
                    (Some(false), _) | (_, Some(false)) => Val::Bool(false),
                    (Some(true), Some(true)) => Val::Bool(true),
                    _ => Val::Null,
                },
                BinOp::Or => match (a, b) {
                    (Some(true), _) | (_, Some(true)) => Val::Bool(true),
                    (Some(false), Some(false)) => Val::Bool(false),
                    _ => Val::Null,
                },
                _ => match (a, b) {
                    (Some(x), Some(y)) => Val::Bool(x != y),
                    _ => Val::Null,
                },
            });
        }
        let a = self.eval(l, row, aggs)?;
        let b = self.eval(r, row, aggs)?;
        let opt = |o: Option<bool>| o.map(Val::Bool).unwrap_or(Val::Null);
        Ok(match op {
            BinOp::Eq => opt(equals(&a, &b)),
            BinOp::Ne => opt(equals(&a, &b).map(|x| !x)),
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let nan = |v: &Val| matches!(v, Val::Float(f) if f.is_nan());
                let numeric = |v: &Val| matches!(v, Val::Int(_) | Val::Float(_));
                if a.is_null() || b.is_null() {
                    Val::Null
                } else if (nan(&a) || nan(&b)) && numeric(&a) && numeric(&b) {
                    // NaN is comparable to numbers and every comparison is false.
                    Val::Bool(false)
                } else {
                    match compare(&a, &b) {
                        None => Val::Null,
                        Some(o) => Val::Bool(match op {
                            BinOp::Lt => o.is_lt(),
                            BinOp::Le => o.is_le(),
                            BinOp::Gt => o.is_gt(),
                            _ => o.is_ge(),
                        }),
                    }
                }
            }
            BinOp::In => match b {
                Val::Null => Val::Null,
                Val::List(items) => {
                    let mut unknown = false;
                    for item in &items {
                        match equals(&a, item) {
                            Some(true) => return Ok(Val::Bool(true)),
                            None => unknown = true,
                            Some(false) => {}
                        }
                    }
                    if unknown {
                        Val::Null
                    } else {
                        Val::Bool(false)
                    }
                }
                other => {
                    return Err(CypherError::type_error(format!(
                        "IN expects a LIST, got {}",
                        other.type_name()
                    )))
                }
            },
            BinOp::StartsWith | BinOp::EndsWith | BinOp::Contains => match (&a, &b) {
                (Val::Str(x), Val::Str(y)) => Val::Bool(match op {
                    BinOp::StartsWith => x.starts_with(y.as_str()),
                    BinOp::EndsWith => x.ends_with(y.as_str()),
                    _ => x.contains(y.as_str()),
                }),
                _ => Val::Null,
            },
            BinOp::Add => match (a, b) {
                (Val::Null, _) | (_, Val::Null) => Val::Null,
                (Val::List(mut x), Val::List(y)) => {
                    x.extend(y);
                    Val::List(x)
                }
                (Val::List(mut x), y) => {
                    x.push(y);
                    Val::List(x)
                }
                (x, Val::List(mut y)) => {
                    y.insert(0, x);
                    Val::List(y)
                }
                (Val::Str(x), y) => Val::Str(x + &display(&y)),
                (x, Val::Str(y)) => Val::Str(display(&x) + &y),
                (x, y) => arith(op, &x, &y)?,
            },
            _ => {
                if a.is_null() || b.is_null() {
                    Val::Null
                } else {
                    arith(op, &a, &b)?
                }
            }
        })
    }

    fn call(&self, name: &str, args: Vec<Val>) -> Result<Val, CypherError> {
        let arity = |n: usize| -> Result<(), CypherError> {
            if args.len() == n {
                Ok(())
            } else {
                Err(CypherError::semantic(format!(
                    "{name}() takes {n} argument(s), got {}",
                    args.len()
                )))
            }
        };
        let a0 = args.first().cloned().unwrap_or(Val::Null);
        let str_fn = |f: &dyn Fn(&str) -> Val| -> Result<Val, CypherError> {
            arity(1)?;
            match &a0 {
                Val::Null => Ok(Val::Null),
                Val::Str(s) => Ok(f(s)),
                other => Err(CypherError::type_error(format!(
                    "{name}() expects a STRING, got {}",
                    other.type_name()
                ))),
            }
        };
        let num_fn = |f: &dyn Fn(f64) -> f64, keep_int: bool| -> Result<Val, CypherError> {
            arity(1)?;
            match &a0 {
                Val::Null => Ok(Val::Null),
                Val::Int(i) if keep_int => Ok(Val::Int(f(*i as f64) as i64)),
                Val::Int(i) => Ok(Val::Float(f(*i as f64))),
                Val::Float(x) => Ok(Val::Float(f(*x))),
                other => Err(CypherError::type_error(format!(
                    "{name}() expects a number, got {}",
                    other.type_name()
                ))),
            }
        };
        if a0.is_null() && args.len() == 1 && !matches!(name, "coalesce" | "exists") {
            return Ok(Val::Null);
        }
        Ok(match name {
            "id" | "elementid" => {
                arity(1)?;
                match a0 {
                    Val::Node(id) | Val::Rel(id) => Val::Str(id),
                    other => {
                        return Err(CypherError::type_error(format!(
                            "{name}() of a {}",
                            other.type_name()
                        )))
                    }
                }
            }
            "labels" => {
                arity(1)?;
                match a0 {
                    Val::Node(id) => Val::List(
                        self.w
                            .node(&id)
                            .map(|n| vec![Val::Str(n.type_name.clone())])
                            .unwrap_or_default(),
                    ),
                    other => {
                        return Err(CypherError::type_error(format!(
                            "labels() of a {}",
                            other.type_name()
                        )))
                    }
                }
            }
            "type" => {
                arity(1)?;
                match a0 {
                    Val::Rel(id) => self
                        .w
                        .edge(&id)
                        .map(|e| Val::Str(e.type_name.clone()))
                        .unwrap_or(Val::Null),
                    other => {
                        return Err(CypherError::type_error(format!(
                            "type() of a {}",
                            other.type_name()
                        )))
                    }
                }
            }
            "properties" => {
                arity(1)?;
                match a0 {
                    Val::Map(m) => Val::Map(m),
                    v @ (Val::Node(_) | Val::Rel(_)) => Val::Map(self.properties_of(&v)),
                    other => {
                        return Err(CypherError::type_error(format!(
                            "properties() of a {}",
                            other.type_name()
                        )))
                    }
                }
            }
            "keys" => {
                arity(1)?;
                let m = match a0 {
                    Val::Map(m) => m,
                    v @ (Val::Node(_) | Val::Rel(_)) => self.properties_of(&v),
                    other => {
                        return Err(CypherError::type_error(format!(
                            "keys() of a {}",
                            other.type_name()
                        )))
                    }
                };
                Val::List(m.into_keys().map(Val::Str).collect())
            }
            "startnode" | "endnode" => {
                arity(1)?;
                match a0 {
                    Val::Rel(id) => self
                        .w
                        .edge(&id)
                        .map(|e| {
                            Val::Node(if name == "startnode" {
                                e.src.clone()
                            } else {
                                e.dst.clone()
                            })
                        })
                        .unwrap_or(Val::Null),
                    other => {
                        return Err(CypherError::type_error(format!(
                            "{name}() of a {}",
                            other.type_name()
                        )))
                    }
                }
            }
            "nodes" | "relationships" | "rels" => {
                arity(1)?;
                match a0 {
                    Val::Path { nodes, rels } => Val::List(if name == "nodes" {
                        nodes.into_iter().map(Val::Node).collect()
                    } else {
                        rels.into_iter().map(Val::Rel).collect()
                    }),
                    other => {
                        return Err(CypherError::type_error(format!(
                            "{name}() of a {}",
                            other.type_name()
                        )))
                    }
                }
            }
            "length" | "size" => {
                arity(1)?;
                match a0 {
                    Val::Path { rels, .. } => Val::Int(rels.len() as i64),
                    Val::List(items) => Val::Int(items.len() as i64),
                    Val::Str(s) => Val::Int(s.chars().count() as i64),
                    Val::Map(m) => Val::Int(m.len() as i64),
                    other => {
                        return Err(CypherError::type_error(format!(
                            "{name}() of a {}",
                            other.type_name()
                        )))
                    }
                }
            }
            "coalesce" => args.into_iter().find(|v| !v.is_null()).unwrap_or(Val::Null),
            "exists" => {
                arity(1)?;
                Val::Bool(!a0.is_null())
            }
            "head" | "last" | "tail" | "reverse" => {
                arity(1)?;
                match a0 {
                    Val::List(mut items) => match name {
                        "head" => items.first().cloned().unwrap_or(Val::Null),
                        "last" => items.last().cloned().unwrap_or(Val::Null),
                        "tail" => Val::List(items.into_iter().skip(1).collect()),
                        _ => {
                            items.reverse();
                            Val::List(items)
                        }
                    },
                    Val::Str(s) if name == "reverse" => Val::Str(s.chars().rev().collect()),
                    other => {
                        return Err(CypherError::type_error(format!(
                            "{name}() of a {}",
                            other.type_name()
                        )))
                    }
                }
            }
            "range" => {
                let ints: Vec<i64> = args
                    .iter()
                    .map(|v| match v {
                        Val::Int(i) => Ok(*i),
                        other => Err(CypherError::type_error(format!(
                            "range() expects integers, got {}",
                            other.type_name()
                        ))),
                    })
                    .collect::<Result<_, _>>()?;
                let (start, end, step) = match ints.as_slice() {
                    [s, e] => (*s, *e, 1),
                    [s, e, st] => (*s, *e, *st),
                    _ => return Err(CypherError::semantic("range() takes 2 or 3 arguments")),
                };
                if step == 0 {
                    return Err(CypherError::semantic("range() step must not be 0"));
                }
                let mut out = Vec::new();
                let mut i = start;
                while (step > 0 && i <= end) || (step < 0 && i >= end) {
                    out.push(Val::Int(i));
                    self.check_rows(out.len())?;
                    i += step;
                }
                Val::List(out)
            }
            "tolower" => str_fn(&|s| Val::Str(s.to_lowercase()))?,
            "toupper" => str_fn(&|s| Val::Str(s.to_uppercase()))?,
            "trim" => str_fn(&|s| Val::Str(s.trim().to_string()))?,
            "ltrim" => str_fn(&|s| Val::Str(s.trim_start().to_string()))?,
            "rtrim" => str_fn(&|s| Val::Str(s.trim_end().to_string()))?,
            "replace" | "split" | "substring" | "left" | "right" => {
                let Val::Str(s) = a0 else {
                    return Err(CypherError::type_error(format!(
                        "{name}() expects a STRING"
                    )));
                };
                let as_int = |v: Option<&Val>| match v {
                    Some(Val::Int(i)) if *i >= 0 => Ok(*i as usize),
                    _ => Err(CypherError::type_error(format!(
                        "{name}() expects a non-negative integer"
                    ))),
                };
                let as_str = |v: Option<&Val>| match v {
                    Some(Val::Str(x)) => Ok(x.clone()),
                    _ => Err(CypherError::type_error(format!(
                        "{name}() expects a STRING"
                    ))),
                };
                match name {
                    "replace" => {
                        arity(3)?;
                        Val::Str(s.replace(&as_str(args.get(1))?, &as_str(args.get(2))?))
                    }
                    "split" => {
                        arity(2)?;
                        Val::List(
                            s.split(as_str(args.get(1))?.as_str())
                                .map(|x| Val::Str(x.to_string()))
                                .collect(),
                        )
                    }
                    "substring" => {
                        let start = as_int(args.get(1))?;
                        let chars = s.chars().skip(start);
                        Val::Str(match args.get(2) {
                            Some(_) => chars.take(as_int(args.get(2))?).collect(),
                            None => chars.collect(),
                        })
                    }
                    "left" => {
                        arity(2)?;
                        Val::Str(s.chars().take(as_int(args.get(1))?).collect())
                    }
                    _ => {
                        arity(2)?;
                        let n = as_int(args.get(1))?;
                        let len = s.chars().count();
                        Val::Str(s.chars().skip(len.saturating_sub(n)).collect())
                    }
                }
            }
            "tostring" => {
                arity(1)?;
                match a0 {
                    Val::Str(s) => Val::Str(s),
                    v @ (Val::Int(_) | Val::Float(_) | Val::Bool(_) | Val::Ts(_)) => {
                        Val::Str(display(&v))
                    }
                    other => {
                        return Err(CypherError::type_error(format!(
                            "toString() of a {}",
                            other.type_name()
                        )))
                    }
                }
            }
            "tointeger" => {
                arity(1)?;
                match a0 {
                    Val::Int(i) => Val::Int(i),
                    Val::Float(f) => Val::Int(f.trunc() as i64),
                    Val::Str(s) => s
                        .trim()
                        .parse::<i64>()
                        .map(Val::Int)
                        .or_else(|_| s.trim().parse::<f64>().map(|f| Val::Int(f.trunc() as i64)))
                        .unwrap_or(Val::Null),
                    Val::Bool(b) => Val::Int(b as i64),
                    other => {
                        return Err(CypherError::type_error(format!(
                            "toInteger() of a {}",
                            other.type_name()
                        )))
                    }
                }
            }
            "tofloat" => {
                arity(1)?;
                match a0 {
                    Val::Int(i) => Val::Float(i as f64),
                    Val::Float(f) => Val::Float(f),
                    Val::Str(s) => s.trim().parse::<f64>().map(Val::Float).unwrap_or(Val::Null),
                    other => {
                        return Err(CypherError::type_error(format!(
                            "toFloat() of a {}",
                            other.type_name()
                        )))
                    }
                }
            }
            "toboolean" => {
                arity(1)?;
                match a0 {
                    Val::Bool(b) => Val::Bool(b),
                    Val::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
                        "true" => Val::Bool(true),
                        "false" => Val::Bool(false),
                        _ => Val::Null,
                    },
                    other => {
                        return Err(CypherError::type_error(format!(
                            "toBoolean() of a {}",
                            other.type_name()
                        )))
                    }
                }
            }
            "abs" => match a0 {
                Val::Int(i) => {
                    arity(1)?;
                    Val::Int(i.abs())
                }
                _ => num_fn(&f64::abs, false)?,
            },
            "ceil" => num_fn(&f64::ceil, false)?,
            "floor" => num_fn(&f64::floor, false)?,
            "round" => num_fn(&f64::round, false)?,
            "sqrt" => num_fn(&f64::sqrt, false)?,
            "sign" => num_fn(&f64::signum, true)?,
            "timestamp" => {
                arity(0)?;
                Val::Int(self.opts.now_ms)
            }
            "rand" => {
                arity(0)?;
                let n = self.rng.get().wrapping_add(1);
                self.rng.set(n);
                let mut h = self.rand.build_hasher();
                h.write_u64(n);
                Val::Float((h.finish() >> 11) as f64 / (1u64 << 53) as f64)
            }
            "eh.active" | "eh.lastseen" | "eh.firstseen" | "eh.source" | "eh.version"
            | "eh.origin" | "eh.executionid" => {
                arity(1)?;
                let meta = match &a0 {
                    Val::Node(id) => self.w.node(id).map(|n| {
                        (
                            n.active,
                            n.last_seen,
                            n.first_seen,
                            n.source.clone(),
                            n.version_updated,
                            n.origin.clone(),
                            n.execution_id.clone(),
                        )
                    }),
                    Val::Rel(id) => self.w.edge(id).map(|e| {
                        (
                            e.active,
                            e.last_seen,
                            e.first_seen,
                            e.source.clone(),
                            e.version_updated,
                            e.origin.clone(),
                            e.execution_id.clone(),
                        )
                    }),
                    other => {
                        return Err(CypherError::type_error(format!(
                            "{name}() of a {}",
                            other.type_name()
                        )))
                    }
                };
                let Some((active, last, first, source, version, origin, exec)) = meta else {
                    return Ok(Val::Null);
                };
                match name {
                    "eh.active" => Val::Bool(active),
                    "eh.lastseen" => Val::Ts(last),
                    "eh.firstseen" => Val::Ts(first),
                    "eh.source" => Val::Str(source),
                    "eh.version" => Val::Int(version as i64),
                    "eh.origin" => origin.map(Val::Str).unwrap_or(Val::Null),
                    _ => Val::Str(exec),
                }
            }
            other => return Err(CypherError::unsupported(&format!("function {other}()"))),
        })
    }

    /// Resolve a value for output, reading element state at the end of the query.
    fn resolve(&self, v: &Val) -> Out {
        let props = |p: &BTreeMap<String, PropValue>| -> BTreeMap<String, Out> {
            p.iter()
                .map(|(k, v)| (k.clone(), self.resolve(&Val::from_prop(v))))
                .collect()
        };
        match v {
            Val::Null => Out::Null,
            Val::Bool(b) => Out::Bool(*b),
            Val::Int(i) => Out::Int(*i),
            Val::Float(f) => Out::Float(*f),
            Val::Str(s) => Out::Str(s.clone()),
            Val::Ts(t) => Out::Ts(*t),
            Val::List(items) => Out::List(items.iter().map(|x| self.resolve(x)).collect()),
            Val::Map(m) => Out::Map(
                m.iter()
                    .map(|(k, v)| (k.clone(), self.resolve(v)))
                    .collect(),
            ),
            Val::Node(id) => match self.w.node(id) {
                Some(n) => Out::Node {
                    id: n.id.clone(),
                    labels: vec![n.type_name.clone()],
                    props: props(&n.props),
                },
                None => Out::Null,
            },
            Val::Rel(id) => match self.w.edge(id) {
                Some(e) => Out::Rel {
                    id: e.id.clone(),
                    type_name: e.type_name.clone(),
                    start: e.src.clone(),
                    end: e.dst.clone(),
                    props: props(&e.props),
                },
                None => Out::Null,
            },
            Val::Path { nodes, rels } => Out::Path {
                nodes: nodes
                    .iter()
                    .map(|n| self.resolve(&Val::Node(n.clone())))
                    .collect(),
                rels: rels
                    .iter()
                    .map(|r| self.resolve(&Val::Rel(r.clone())))
                    .collect(),
            },
        }
    }
}

fn diff_count(a: &BTreeMap<String, PropValue>, b: &BTreeMap<String, PropValue>) -> u64 {
    let changed = b.iter().filter(|(k, v)| a.get(*k) != Some(*v)).count();
    let removed = a.keys().filter(|k| !b.contains_key(*k)).count();
    (changed + removed) as u64
}

fn display(v: &Val) -> String {
    match v {
        Val::Str(s) => s.clone(),
        Val::Int(i) => i.to_string(),
        Val::Float(f) => {
            if f.fract() == 0.0 && f.is_finite() {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        Val::Bool(b) => b.to_string(),
        Val::Ts(t) => match PropValue::Timestamp(*t).to_json() {
            Json::Object(m) => m
                .get("$timestamp")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            _ => t.to_string(),
        },
        Val::Null => "null".into(),
        other => format!("{other:?}"),
    }
}

fn arith(op: BinOp, a: &Val, b: &Val) -> Result<Val, CypherError> {
    let overflow = || CypherError::type_error("integer overflow");
    Ok(match (a, b) {
        (Val::Int(x), Val::Int(y)) => match op {
            BinOp::Add => Val::Int(x.checked_add(*y).ok_or_else(overflow)?),
            BinOp::Sub => Val::Int(x.checked_sub(*y).ok_or_else(overflow)?),
            BinOp::Mul => Val::Int(x.checked_mul(*y).ok_or_else(overflow)?),
            BinOp::Div => {
                if *y == 0 {
                    return Err(CypherError::type_error("division by zero"));
                }
                Val::Int(x / y)
            }
            BinOp::Mod => {
                if *y == 0 {
                    return Err(CypherError::type_error("division by zero"));
                }
                Val::Int(x % y)
            }
            BinOp::Pow => Val::Float((*x as f64).powf(*y as f64)),
            _ => unreachable!(),
        },
        (Val::Int(_) | Val::Float(_), Val::Int(_) | Val::Float(_)) => {
            let x = match a {
                Val::Int(i) => *i as f64,
                Val::Float(f) => *f,
                _ => unreachable!(),
            };
            let y = match b {
                Val::Int(i) => *i as f64,
                Val::Float(f) => *f,
                _ => unreachable!(),
            };
            Val::Float(match op {
                BinOp::Add => x + y,
                BinOp::Sub => x - y,
                BinOp::Mul => x * y,
                BinOp::Div => x / y,
                BinOp::Mod => x % y,
                BinOp::Pow => x.powf(y),
                _ => unreachable!(),
            })
        }
        _ => {
            return Err(CypherError::type_error(format!(
                "cannot apply {op:?} to {} and {}",
                a.type_name(),
                b.type_name()
            )))
        }
    })
}

fn pattern_vars(part: &PatternPart) -> Vec<String> {
    let mut vars = Vec::new();
    if let Some(v) = &part.path_var {
        vars.push(v.clone());
    }
    if let Some(v) = &part.start.var {
        vars.push(v.clone());
    }
    for (r, n) in &part.chain {
        if let Some(v) = &r.var {
            vars.push(v.clone());
        }
        if let Some(v) = &n.var {
            vars.push(v.clone());
        }
    }
    vars
}

fn rewrite_aggs(e: &Expr, aggs: &mut Vec<Expr>) -> Expr {
    let mut rw = |x: &Expr| rewrite_aggs(x, aggs);
    match e {
        Expr::CountStar => {
            aggs.push(e.clone());
            Expr::AggRef(aggs.len() - 1)
        }
        Expr::Func { name, .. } if AGGREGATES.contains(&name.as_str()) => {
            aggs.push(e.clone());
            Expr::AggRef(aggs.len() - 1)
        }
        Expr::Func {
            name,
            distinct,
            args,
        } => Expr::Func {
            name: name.clone(),
            distinct: *distinct,
            args: args.iter().map(&mut rw).collect(),
        },
        Expr::Prop(b, k) => Expr::Prop(Box::new(rw(b)), k.clone()),
        Expr::Index(a, b) => Expr::Index(Box::new(rw(a)), Box::new(rw(b))),
        Expr::Slice(a, b, c) => Expr::Slice(
            Box::new(rw(a)),
            b.as_ref().map(|x| Box::new(rw(x))),
            c.as_ref().map(|x| Box::new(rw(x))),
        ),
        Expr::List(items) => Expr::List(items.iter().map(&mut rw).collect()),
        Expr::Map(items) => Expr::Map(items.iter().map(|(k, v)| (k.clone(), rw(v))).collect()),
        Expr::Unary(op, x) => Expr::Unary(*op, Box::new(rw(x))),
        Expr::Binary(op, a, b) => Expr::Binary(*op, Box::new(rw(a)), Box::new(rw(b))),
        Expr::IsNull(x, n) => Expr::IsNull(Box::new(rw(x)), *n),
        Expr::HasLabels(x, l) => Expr::HasLabels(Box::new(rw(x)), l.clone()),
        Expr::Case {
            operand,
            whens,
            else_,
        } => Expr::Case {
            operand: operand.as_ref().map(|x| Box::new(rw(x))),
            whens: whens.iter().map(|(a, b)| (rw(a), rw(b))).collect(),
            else_: else_.as_ref().map(|x| Box::new(rw(x))),
        },
        Expr::ListComp {
            var,
            list,
            pred,
            proj,
        } => Expr::ListComp {
            var: var.clone(),
            list: Box::new(rw(list)),
            pred: pred.as_ref().map(|x| Box::new(rw(x))),
            proj: proj.as_ref().map(|x| Box::new(rw(x))),
        },
        Expr::Quantifier {
            kind,
            var,
            list,
            pred,
        } => Expr::Quantifier {
            kind: *kind,
            var: var.clone(),
            list: Box::new(rw(list)),
            pred: Box::new(rw(pred)),
        },
        Expr::Lit(_)
        | Expr::Param(_)
        | Expr::Var(_)
        | Expr::AggRef(_)
        | Expr::Exists { .. }
        | Expr::PatternComp { .. } => e.clone(),
    }
}
