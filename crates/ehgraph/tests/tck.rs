//! openCypher TCK runner (ignored by default; see `scripts/tck.sh`).
//!
//! Reads the Gherkin feature files of an openCypher checkout from `EHGRAPH_TCK_DIR`
//! (the `tck` directory, or the repository root) and runs every scenario end to end
//! through the real write path (`EhGraph::cypher_as` → `Engine::run_cypher` → store
//! commit), one fresh namespace per scenario. Writes a scenario-level JSON report to
//! `EHGRAPH_TCK_OUT` (default `target/tck-results.json`) and prints a summary.
//!
//! Every normalization and classification rule applied here is documented in
//! `docs/wiki/Cypher.md` ("TCK conformance"). In short:
//!
//! - Nodes compare by label set + properties, relationships by type + properties,
//!   paths by their node/relationship sequence and direction. ehgraph element ids
//!   are element identity and are never compared (they are not properties).
//! - Integers and floats are distinct (`1` ≠ `1.0`); floats compare exactly, NaN = NaN.
//! - Side effects are a diff of the visible graph before and after the query
//!   (as the TCK defines them), not ehgraph's `stats` counters.
//! - Expected errors pass when ehgraph raises an error of a mapped kind; the phase
//!   (compile time / runtime) and the detail code are not checked.
//! - OUT-OF-MODEL scenarios are skipped by the conservative rules in `static_oom`
//!   and `dynamic_oom` and are counted separately from PASS and FAIL.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ehgraph::{Caller, EhGraph, GraphConfig};
use ehgraph_cypher::{kind_of_message, ErrorKind, Out};
use serde_json::{json, Value as Json};

// ---------------------------------------------------------------------------
// Gherkin

#[derive(Debug, Clone)]
struct Step {
    text: String,
    doc: Option<String>,
    table: Vec<Vec<String>>,
}

#[derive(Debug, Clone)]
struct Scenario {
    file: String,
    feature: String,
    name: String,
    tags: Vec<String>,
    steps: Vec<Step>,
}

fn split_row(line: &str) -> Vec<String> {
    let line = line.trim();
    let inner = line.strip_prefix('|').unwrap_or(line);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('|') => cur.push('|'),
                Some('n') => cur.push('\n'),
                Some('\\') => cur.push('\\'),
                Some(other) => {
                    cur.push('\\');
                    cur.push(other);
                }
                None => cur.push('\\'),
            },
            '|' => {
                cells.push(cur.trim().to_string());
                cur.clear();
            }
            other => cur.push(other),
        }
    }
    cells
}

fn substitute(s: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = s.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("<{k}>"), v);
    }
    out
}

fn parse_feature(path: &Path, rel: &str) -> Vec<Scenario> {
    let text = std::fs::read_to_string(path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let mut feature = String::new();
    let mut background: Vec<Step> = Vec::new();
    let mut out = Vec::new();

    // Current block: (name, tags, outline, steps, examples)
    struct Block {
        name: String,
        tags: Vec<String>,
        outline: bool,
        background: bool,
        steps: Vec<Step>,
        examples: Vec<Vec<Vec<String>>>,
    }
    let mut cur: Option<Block> = None;
    let mut pending_tags: Vec<String> = Vec::new();
    let mut in_examples = false;

    let flush = |b: Block, background: &mut Vec<Step>, out: &mut Vec<Scenario>, feature: &str| {
        if b.background {
            *background = b.steps;
            return;
        }
        if b.outline {
            let mut n = 0;
            for table in &b.examples {
                if table.is_empty() {
                    continue;
                }
                let header = &table[0];
                for row in &table[1..] {
                    n += 1;
                    let vars: BTreeMap<String, String> =
                        header.iter().cloned().zip(row.iter().cloned()).collect();
                    let steps = background
                        .iter()
                        .chain(b.steps.iter())
                        .map(|s| Step {
                            text: substitute(&s.text, &vars),
                            doc: s.doc.as_ref().map(|d| substitute(d, &vars)),
                            table: s
                                .table
                                .iter()
                                .map(|r| r.iter().map(|c| substitute(c, &vars)).collect())
                                .collect(),
                        })
                        .collect();
                    out.push(Scenario {
                        file: String::new(),
                        feature: feature.to_string(),
                        name: format!("{} (example {n})", b.name),
                        tags: b.tags.clone(),
                        steps,
                    });
                }
            }
        } else {
            let steps = background.iter().chain(b.steps.iter()).cloned().collect();
            out.push(Scenario {
                file: String::new(),
                feature: feature.to_string(),
                name: b.name,
                tags: b.tags,
                steps,
            });
        }
    };

    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let line = raw.trim();
        i += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('@') {
            pending_tags.extend(line.split_whitespace().map(str::to_string));
            continue;
        }
        if let Some(rest) = line.strip_prefix("Feature:") {
            feature = rest.trim().to_string();
            pending_tags.clear();
            continue;
        }
        let header = [
            ("Background:", false, true),
            ("Scenario Outline:", true, false),
            ("Scenario Template:", true, false),
            ("Scenario:", false, false),
        ]
        .iter()
        .find_map(|(kw, outline, bg)| line.strip_prefix(kw).map(|r| (r, *outline, *bg)));
        if let Some((rest, outline, bg)) = header {
            if let Some(b) = cur.take() {
                flush(b, &mut background, &mut out, &feature);
            }
            cur = Some(Block {
                name: rest.trim().to_string(),
                tags: std::mem::take(&mut pending_tags),
                outline,
                background: bg,
                steps: Vec::new(),
                examples: Vec::new(),
            });
            in_examples = false;
            continue;
        }
        if line.starts_with("Examples:") || line.starts_with("Scenarios:") {
            if let Some(b) = cur.as_mut() {
                b.examples.push(Vec::new());
            }
            in_examples = true;
            pending_tags.clear();
            continue;
        }
        let Some(b) = cur.as_mut() else { continue };
        if line.starts_with('|') {
            let row = split_row(line);
            if in_examples {
                if let Some(t) = b.examples.last_mut() {
                    t.push(row);
                }
            } else if let Some(s) = b.steps.last_mut() {
                s.table.push(row);
            }
            continue;
        }
        if line.starts_with("\"\"\"") {
            let indent = raw.len() - raw.trim_start().len();
            let mut doc = Vec::new();
            while i < lines.len() && !lines[i].trim().starts_with("\"\"\"") {
                let l = lines[i];
                let strip = l
                    .char_indices()
                    .take_while(|(idx, c)| *idx < indent && c.is_whitespace())
                    .count();
                doc.push(&l[strip..]);
                i += 1;
            }
            i += 1;
            if let Some(s) = b.steps.last_mut() {
                s.doc = Some(doc.join("\n"));
            }
            continue;
        }
        for kw in ["Given ", "When ", "Then ", "And ", "But "] {
            if let Some(rest) = line.strip_prefix(kw) {
                in_examples = false;
                b.steps.push(Step {
                    text: rest.trim().to_string(),
                    doc: None,
                    table: Vec::new(),
                });
                break;
            }
        }
    }
    if let Some(b) = cur.take() {
        flush(b, &mut background, &mut out, &feature);
    }
    for s in &mut out {
        s.file = rel.to_string();
    }
    out
}

// ---------------------------------------------------------------------------
// TCK value syntax

#[derive(Debug, Clone, PartialEq)]
enum TVal {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<TVal>),
    Map(BTreeMap<String, TVal>),
    Node {
        labels: BTreeSet<String>,
        props: BTreeMap<String, TVal>,
    },
    Rel {
        ty: String,
        props: BTreeMap<String, TVal>,
    },
    /// Nodes, and for each hop the relationship and whether it points forward.
    Path {
        nodes: Vec<TVal>,
        rels: Vec<(TVal, bool)>,
    },
}

struct VP<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> VP<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            s: s.as_bytes(),
            i: 0,
        }
    }
    fn ws(&mut self) {
        while self.i < self.s.len() && (self.s[self.i] as char).is_whitespace() {
            self.i += 1;
        }
    }
    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.i).copied()
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(format!(
                "expected `{}` at {} in {:?}",
                c as char,
                self.i,
                String::from_utf8_lossy(self.s)
            ))
        }
    }
    fn starts(&mut self, t: &str) -> bool {
        self.ws();
        self.s[self.i..].starts_with(t.as_bytes())
    }
    fn ident(&mut self) -> Result<String, String> {
        self.ws();
        if self.eat(b'`') {
            let start = self.i;
            while self.i < self.s.len() && self.s[self.i] != b'`' {
                self.i += 1;
            }
            let id = String::from_utf8_lossy(&self.s[start..self.i]).to_string();
            self.i += 1;
            return Ok(id);
        }
        let start = self.i;
        while self.i < self.s.len()
            && ((self.s[self.i] as char).is_alphanumeric() || self.s[self.i] == b'_')
        {
            self.i += 1;
        }
        if start == self.i {
            return Err(format!("expected identifier at {}", self.i));
        }
        Ok(String::from_utf8_lossy(&self.s[start..self.i]).to_string())
    }
    fn map_body(&mut self) -> Result<BTreeMap<String, TVal>, String> {
        let mut m = BTreeMap::new();
        if self.eat(b'}') {
            return Ok(m);
        }
        loop {
            let k = self.ident()?;
            self.expect(b':')?;
            let v = self.value()?;
            m.insert(k, v);
            if self.eat(b',') {
                continue;
            }
            self.expect(b'}')?;
            return Ok(m);
        }
    }
    fn node(&mut self) -> Result<TVal, String> {
        self.expect(b'(')?;
        let mut labels = BTreeSet::new();
        while self.eat(b':') {
            labels.insert(self.ident()?);
        }
        let props = if self.eat(b'{') {
            self.map_body()?
        } else {
            BTreeMap::new()
        };
        self.expect(b')')?;
        Ok(TVal::Node { labels, props })
    }
    fn rel(&mut self) -> Result<TVal, String> {
        self.expect(b'[')?;
        self.expect(b':')?;
        let ty = self.ident()?;
        let props = if self.eat(b'{') {
            self.map_body()?
        } else {
            BTreeMap::new()
        };
        self.expect(b']')?;
        Ok(TVal::Rel { ty, props })
    }
    fn string(&mut self) -> Result<String, String> {
        let q = self.s[self.i];
        self.i += 1;
        let mut out = Vec::new();
        while self.i < self.s.len() && self.s[self.i] != q {
            if self.s[self.i] == b'\\' && self.i + 1 < self.s.len() {
                self.i += 1;
                out.push(match self.s[self.i] {
                    b'n' => b'\n',
                    b't' => b'\t',
                    b'r' => b'\r',
                    other => other,
                });
            } else {
                out.push(self.s[self.i]);
            }
            self.i += 1;
        }
        self.i += 1;
        Ok(String::from_utf8_lossy(&out).to_string())
    }
    fn value(&mut self) -> Result<TVal, String> {
        match self.peek() {
            None => Err("unexpected end of value".into()),
            Some(b'\'') | Some(b'"') => Ok(TVal::Str(self.string()?)),
            Some(b'[') => {
                // A relationship `[:T ...]` or a list.
                let save = self.i;
                self.i += 1;
                if self.peek() == Some(b':') {
                    self.i = save;
                    return self.rel();
                }
                let mut items = Vec::new();
                if self.eat(b']') {
                    return Ok(TVal::List(items));
                }
                loop {
                    items.push(self.value()?);
                    if self.eat(b',') {
                        continue;
                    }
                    self.expect(b']')?;
                    return Ok(TVal::List(items));
                }
            }
            Some(b'{') => {
                self.i += 1;
                Ok(TVal::Map(self.map_body()?))
            }
            Some(b'(') => self.node(),
            Some(b'<') => {
                self.i += 1;
                let mut nodes = vec![self.node()?];
                let mut rels = Vec::new();
                loop {
                    if self.eat(b'>') {
                        return Ok(TVal::Path { nodes, rels });
                    }
                    if self.eat(b'<') {
                        self.expect(b'-')?;
                        let r = self.rel()?;
                        self.expect(b'-')?;
                        rels.push((r, false));
                    } else {
                        self.expect(b'-')?;
                        let r = self.rel()?;
                        self.expect(b'-')?;
                        self.expect(b'>')?;
                        rels.push((r, true));
                    }
                    nodes.push(self.node()?);
                }
            }
            Some(_) => {
                if self.starts("null") {
                    self.i += 4;
                    return Ok(TVal::Null);
                }
                if self.starts("true") {
                    self.i += 4;
                    return Ok(TVal::Bool(true));
                }
                if self.starts("false") {
                    self.i += 5;
                    return Ok(TVal::Bool(false));
                }
                if self.starts("NaN") {
                    self.i += 3;
                    return Ok(TVal::Float(f64::NAN));
                }
                for (t, v) in [
                    ("-Infinity", f64::NEG_INFINITY),
                    ("Infinity", f64::INFINITY),
                    ("-Inf", f64::NEG_INFINITY),
                    ("Inf", f64::INFINITY),
                ] {
                    if self.starts(t) {
                        self.i += t.len();
                        return Ok(TVal::Float(v));
                    }
                }
                let start = self.i;
                while self.i < self.s.len()
                    && matches!(
                        self.s[self.i],
                        b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'
                    )
                {
                    self.i += 1;
                }
                let t = String::from_utf8_lossy(&self.s[start..self.i]).to_string();
                if let Ok(i) = t.parse::<i64>() {
                    return Ok(TVal::Int(i));
                }
                if let Ok(f) = t.parse::<f64>() {
                    return Ok(TVal::Float(f));
                }
                Err(format!(
                    "cannot parse value at {start}: {:?}",
                    String::from_utf8_lossy(self.s)
                ))
            }
        }
    }
}

fn parse_value(s: &str) -> Result<TVal, String> {
    let mut p = VP::new(s);
    let v = p.value()?;
    p.ws();
    if p.i != p.s.len() {
        return Err(format!("trailing input in value {s:?}"));
    }
    Ok(v)
}

fn to_json(v: &TVal) -> Json {
    match v {
        TVal::Null => Json::Null,
        TVal::Bool(b) => json!(b),
        TVal::Int(i) => json!(i),
        TVal::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        TVal::Str(s) => json!(s),
        TVal::List(items) => Json::Array(items.iter().map(to_json).collect()),
        TVal::Map(m) => Json::Object(m.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()),
        other => panic!("graph element as a parameter: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Comparison

fn eq_props(a: &BTreeMap<String, Out>, b: &BTreeMap<String, TVal>, unordered: bool) -> bool {
    a.len() == b.len()
        && a.iter()
            .all(|(k, v)| b.get(k).is_some_and(|e| matches_val(v, e, unordered)))
}

fn matches_val(out: &Out, exp: &TVal, unordered: bool) -> bool {
    match (out, exp) {
        (Out::Null, TVal::Null) => true,
        (Out::Bool(a), TVal::Bool(b)) => a == b,
        (Out::Int(a), TVal::Int(b)) => a == b,
        (Out::Float(a), TVal::Float(b)) => a == b || (a.is_nan() && b.is_nan()),
        (Out::Str(a), TVal::Str(b)) => a == b,
        (Out::List(a), TVal::List(b)) => {
            if a.len() != b.len() {
                return false;
            }
            if unordered {
                multiset_match(a, b, |x, y| matches_val(x, y, unordered))
            } else {
                a.iter().zip(b).all(|(x, y)| matches_val(x, y, unordered))
            }
        }
        (Out::Map(a), TVal::Map(b)) => eq_props(a, b, unordered),
        (
            Out::Node { labels, props, .. },
            TVal::Node {
                labels: el,
                props: ep,
            },
        ) => {
            labels.iter().cloned().collect::<BTreeSet<_>>() == *el && eq_props(props, ep, unordered)
        }
        (
            Out::Rel {
                type_name, props, ..
            },
            TVal::Rel { ty, props: ep },
        ) => type_name == ty && eq_props(props, ep, unordered),
        (
            Out::Path { nodes, rels },
            TVal::Path {
                nodes: en,
                rels: er,
            },
        ) => {
            if nodes.len() != en.len() || rels.len() != er.len() {
                return false;
            }
            if !nodes
                .iter()
                .zip(en)
                .all(|(a, b)| matches_val(a, b, unordered))
            {
                return false;
            }
            rels.iter().enumerate().all(|(i, r)| {
                let (exp_r, forward) = &er[i];
                let from = match &nodes[i] {
                    Out::Node { id, .. } => id.clone(),
                    _ => return false,
                };
                let ok_dir = match r {
                    Out::Rel { start, end, .. } => {
                        if *forward {
                            *start == from
                        } else {
                            *end == from
                        }
                    }
                    _ => false,
                };
                ok_dir && matches_val(r, exp_r, unordered)
            })
        }
        _ => false,
    }
}

fn multiset_match<A, B>(a: &[A], b: &[B], eq: impl Fn(&A, &B) -> bool) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut used = vec![false; b.len()];
    'outer: for x in a {
        for (j, y) in b.iter().enumerate() {
            if !used[j] && eq(x, y) {
                used[j] = true;
                continue 'outer;
            }
        }
        return false;
    }
    true
}

fn show_out(v: &Out) -> String {
    match v {
        Out::Null => "null".into(),
        Out::Bool(b) => b.to_string(),
        Out::Int(i) => i.to_string(),
        Out::Float(f) => format!("{f:?}"),
        Out::Str(s) => format!("'{s}'"),
        Out::Ts(t) => format!("ts({t})"),
        Out::List(items) => format!(
            "[{}]",
            items.iter().map(show_out).collect::<Vec<_>>().join(", ")
        ),
        Out::Map(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{k}: {}", show_out(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Out::Node { labels, props, .. } => format!(
            "({}{})",
            labels.iter().map(|l| format!(":{l}")).collect::<String>(),
            if props.is_empty() {
                String::new()
            } else {
                format!(" {}", show_out(&Out::Map(props.clone())))
            }
        ),
        Out::Rel {
            type_name, props, ..
        } => format!(
            "[:{type_name}{}]",
            if props.is_empty() {
                String::new()
            } else {
                format!(" {}", show_out(&Out::Map(props.clone())))
            }
        ),
        Out::Path { nodes, rels } => format!(
            "<{}>",
            nodes
                .iter()
                .map(show_out)
                .zip(
                    rels.iter()
                        .map(show_out)
                        .chain(std::iter::once(String::new()))
                )
                .map(|(n, r)| if r.is_empty() { n } else { format!("{n}-{r}-") })
                .collect::<String>()
        ),
    }
}

// ---------------------------------------------------------------------------
// Running

/// A scenario status and, for a pass via an error mapping, which mapping.
type Verdict = (Status, Option<String>);

type QResult = Result<(Vec<String>, Vec<Vec<Out>>), (Option<ErrorKind>, String)>;

struct Ctx<'a> {
    graph: &'a EhGraph,
    caller: Caller,
    ns: String,
    params: serde_json::Map<String, Json>,
}

impl Ctx<'_> {
    fn run(&self, query: &str, params: bool) -> QResult {
        let args = json!({
            "namespace": self.ns,
            "query": query,
            "params": if params { Json::Object(self.params.clone()) } else { json!({}) },
            "max_rows": 1_000_000,
        });
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.graph.cypher_as(&self.caller, &args)
        }));
        match res {
            Ok(Ok(run)) => Ok((run.outcome.columns, run.outcome.rows)),
            Ok(Err(e)) => Err((kind_of_message(&e.message), e.message)),
            Err(_) => Err((None, "PANIC in engine".into())),
        }
    }

    /// Visible graph state, as the TCK measures side effects.
    fn snapshot(&self) -> Result<State, String> {
        let mut st = State::default();
        let (_, rows) = self.run("MATCH (n) RETURN n", false).map_err(|e| e.1)?;
        for row in rows {
            if let Some(Out::Node { id, labels, props }) = row.first() {
                st.nodes.insert(id.clone());
                for l in labels {
                    st.labels.insert(l.clone());
                }
                for (k, v) in props {
                    st.props.insert((format!("n:{id}"), k.clone(), show_out(v)));
                }
            }
        }
        let (_, rows) = self
            .run("MATCH ()-[r]->() RETURN r", false)
            .map_err(|e| e.1)?;
        for row in rows {
            if let Some(Out::Rel { id, props, .. }) = row.first() {
                st.rels.insert(id.clone());
                for (k, v) in props {
                    st.props.insert((format!("r:{id}"), k.clone(), show_out(v)));
                }
            }
        }
        Ok(st)
    }
}

#[derive(Default, Debug)]
struct State {
    nodes: BTreeSet<String>,
    rels: BTreeSet<String>,
    labels: BTreeSet<String>,
    props: BTreeSet<(String, String, String)>,
}

fn side_effects(before: &State, after: &State) -> BTreeMap<&'static str, usize> {
    let mut m = BTreeMap::new();
    m.insert("+nodes", after.nodes.difference(&before.nodes).count());
    m.insert("-nodes", before.nodes.difference(&after.nodes).count());
    m.insert(
        "+relationships",
        after.rels.difference(&before.rels).count(),
    );
    m.insert(
        "-relationships",
        before.rels.difference(&after.rels).count(),
    );
    m.insert("+properties", after.props.difference(&before.props).count());
    m.insert("-properties", before.props.difference(&after.props).count());
    m.insert("+labels", after.labels.difference(&before.labels).count());
    // Label names present before and absent after, graph-wide (as for +labels).
    m.insert("-labels", before.labels.difference(&after.labels).count());
    m
}

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Pass,
    Fail(String),
    Oom(String),
}

/// Static out-of-model rules, on the scenario text. Conservative: each one names
/// a documented model limit (docs/wiki/Cypher.md §1/§4).
fn static_oom(sc: &Scenario) -> Option<String> {
    let mut queries = String::new();
    let mut expected = String::new();
    for s in &sc.steps {
        if s.text.starts_with("there exists a procedure") {
            return Some("procedures (CALL)".into());
        }
        if let Some(d) = &s.doc {
            queries.push_str(d);
            queries.push('\n');
        }
        for row in &s.table {
            expected.push_str(&row.join(" | "));
            expected.push('\n');
        }
    }
    let q = strip_strings(&queries);
    let lower = q.to_ascii_lowercase();
    // `name(` or `name.sub(` as a whole word, not a property access.
    let word_call = |name: &str| -> bool {
        let b = lower.as_bytes();
        lower.match_indices(name).any(|(at, _)| {
            let before_ok = at == 0 || {
                let c = b[at - 1] as char;
                !(c.is_alphanumeric() || c == '_' || c == '.')
            };
            let mut rest = &lower[at + name.len()..];
            if rest.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
                return false;
            }
            if let Some(r) = rest.strip_prefix('.') {
                rest = r.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_');
            }
            before_ok && rest.trim_start().starts_with('(')
        })
    };
    for f in [
        "date",
        "datetime",
        "localdatetime",
        "time",
        "localtime",
        "duration",
        "point",
        "distance",
    ] {
        if word_call(f) {
            return Some("temporal/spatial types".into());
        }
    }
    if lower.contains("create index")
        || lower.contains("create constraint")
        || lower.contains("drop index")
        || lower.contains("drop constraint")
    {
        return Some("schema DDL".into());
    }
    if has_call_procedure(&q) {
        return Some("procedures (CALL)".into());
    }
    if has_multi_label(&q) || has_multi_label(&strip_strings(&expected)) {
        return Some("multiple labels per node".into());
    }
    None
}

/// The scenario uses a property literally named `id`, which ehgraph reserves for
/// the element id. Not an exclusion: failures are only tagged with it.
fn uses_id_property(sc: &Scenario) -> bool {
    sc.steps.iter().any(|s| {
        s.doc
            .as_deref()
            .is_some_and(|d| has_id_property(&strip_strings(d)))
            || s.table
                .iter()
                .any(|r| has_id_property(&strip_strings(&r.join(" | "))))
    })
}

/// Replace string literal contents with `_` so rules do not fire inside strings.
fn strip_strings(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' {
                    chars.next();
                    out.push('_');
                } else if c == q {
                    quote = None;
                    out.push(c);
                } else {
                    out.push('_');
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                }
                out.push(c);
            }
        }
    }
    out
}

fn has_call_procedure(q: &str) -> bool {
    let up = q.to_ascii_uppercase();
    let mut start = 0;
    while let Some(pos) = up[start..].find("CALL") {
        let at = start + pos;
        let before_ok = at == 0 || !(up.as_bytes()[at - 1] as char).is_alphanumeric();
        let rest = up[at + 4..].trim_start();
        if before_ok
            && up[at + 4..].starts_with(char::is_whitespace)
            && rest.starts_with(|c: char| c.is_ascii_alphabetic())
        {
            return true;
        }
        start = at + 4;
    }
    false
}

/// A node pattern or label test with two or more labels, or a label expression.
fn has_multi_label(q: &str) -> bool {
    let b = q.as_bytes();
    let ident = |i: &mut usize| {
        let s = *i;
        if *i < b.len() && b[*i] == b'`' {
            *i += 1;
            while *i < b.len() && b[*i] != b'`' {
                *i += 1;
            }
            *i += 1;
        } else {
            while *i < b.len() && ((b[*i] as char).is_alphanumeric() || b[*i] == b'_') {
                *i += 1;
            }
        }
        *i > s
    };
    let mut i = 0;
    while i < b.len() {
        // A variable (or nothing) immediately followed by `:Label:Label`.
        if b[i] == b':' && i > 0 {
            let prev = b[i - 1] as char;
            if prev.is_alphanumeric() || prev == '_' || prev == '(' || prev == '`' {
                let mut j = i + 1;
                if ident(&mut j) {
                    if j < b.len() && b[j] == b':' {
                        let mut k = j + 1;
                        if ident(&mut k) {
                            // Exclude `[r:T]`-style relationship types: they cannot chain.
                            return true;
                        }
                    }
                    if j < b.len() && (b[j] == b'&' || b[j] == b'|') {
                        let opening = q[..i].rfind(['(', '[']);
                        if opening.is_some_and(|o| b[o] == b'(') {
                            return true;
                        }
                    }
                }
            }
        }
        i += 1;
    }
    false
}

/// A property literally named `id` (ehgraph's reserved element id).
fn has_id_property(q: &str) -> bool {
    let b = q.as_bytes();
    for (i, _) in q.match_indices("id") {
        let before = if i == 0 { b' ' } else { b[i - 1] };
        let after = b.get(i + 2).copied().unwrap_or(b' ');
        if (after as char).is_alphanumeric() || after == b'_' {
            continue;
        }
        if before == b'.' {
            return true;
        }
        if (before as char).is_alphanumeric() || before == b'_' {
            continue;
        }
        // `{id: …}` / `, id: …` map keys
        let rest = q[i + 2..].trim_start();
        let lead = q[..i].trim_end();
        if rest.starts_with(':') && (lead.ends_with('{') || lead.ends_with(',')) {
            return true;
        }
    }
    false
}

/// Errors that name a genuine model limit, when success was expected. An unlabeled
/// node (`CREATE ()`) is NOT excluded: it is counted as a FAIL.
fn dynamic_oom(msg: &str) -> Option<String> {
    if msg.contains("carry exactly one label") && !msg.contains("got 0") {
        return Some("multiple labels per node".into());
    }
    if msg.contains("adding a second label") {
        return Some("multiple labels per node".into());
    }
    if msg.contains("one edge per (start, type, end)") {
        return Some("parallel relationships of one type".into());
    }
    None
}

/// TCK error class → ehgraph error kinds that count as a match.
fn error_matches(expected: &str, detail: &str, got: Option<ErrorKind>) -> bool {
    use ErrorKind::*;
    let Some(got) = got else { return false };
    let ok: &[ErrorKind] = match expected {
        // The TCK raises SyntaxError for semantic analysis too (UndefinedVariable,
        // VariableAlreadyBound, InvalidAggregation, ...).
        "SyntaxError" => &[Syntax, Unsupported, Semantic],
        "SemanticError" => &[Semantic, Syntax],
        "TypeError" => &[Type],
        "ArgumentError" => &[Type, Semantic, Limit],
        "ConstraintVerificationFailed" => &[Constraint],
        "EntityNotFound" => &[Semantic, Type, Constraint],
        "ParameterMissing" => &[Semantic, Syntax],
        _ => &[],
    };
    // A compile-time SyntaxError with detail InvalidArgumentType is a type check
    // (`WHERE 1`, `NOT 'x'`) that ehgraph only performs at runtime.
    ok.contains(&got)
        || (expected == "SyntaxError" && detail == "InvalidArgumentType" && got == Type)
}

fn run_scenario(ctx: &mut Ctx, sc: &Scenario, tck: &Path) -> (Status, Option<String>) {
    let mut result: Option<QResult> = None;
    let mut before: Option<State> = None;
    let unexpected = |msg: &str, during: &str| -> Status {
        match dynamic_oom(msg) {
            Some(r) => Status::Oom(r),
            None => Status::Fail(format!("{during}: {msg}")),
        }
    };
    let mut via: Option<String> = None;
    for step in &sc.steps {
        let t = step.text.as_str();
        if t == "an empty graph" || t == "any graph" {
            continue;
        }
        if let Some(name) = t
            .strip_prefix("the ")
            .and_then(|r| r.strip_suffix(" graph"))
        {
            let path = tck.join("graphs").join(name).join(format!("{name}.cypher"));
            let q = std::fs::read_to_string(&path).unwrap_or_default();
            if let Err((_, msg)) = ctx.run(&q, false) {
                return (unexpected(&msg, "named graph"), None);
            }
            continue;
        }
        if t.starts_with("having executed") {
            if let Err((_, msg)) = ctx.run(step.doc.as_deref().unwrap_or(""), false) {
                return (unexpected(&msg, "setup"), None);
            }
            continue;
        }
        if t.starts_with("parameters are") {
            for row in &step.table {
                if row.len() >= 2 {
                    match parse_value(&row[1]) {
                        Ok(v) => {
                            ctx.params.insert(row[0].clone(), to_json(&v));
                        }
                        Err(e) => return (Status::Fail(format!("harness: {e}")), None),
                    }
                }
            }
            continue;
        }
        if t.starts_with("executing query") || t.starts_with("executing control query") {
            before = match ctx.snapshot() {
                Ok(s) => Some(s),
                Err(e) => return (Status::Fail(format!("snapshot: {e}")), None),
            };
            result = Some(ctx.run(step.doc.as_deref().unwrap_or(""), true));
            continue;
        }
        let Some(res) = result.as_ref() else {
            return (
                Status::Fail(format!("harness: step before a query: {t}")),
                None,
            );
        };
        if let Some(rest) = t.strip_prefix("a ").or_else(|| t.strip_prefix("an ")) {
            if rest.contains(" should be raised") {
                let class = rest.split_whitespace().next().unwrap_or("");
                let detail = rest.rsplit(':').next().unwrap_or("").trim();
                return match res {
                    Ok((_, rows)) => (
                        Status::Fail(format!(
                            "expected {class} ({detail}), query succeeded ({} rows)",
                            rows.len()
                        )),
                        None,
                    ),
                    Err((kind, msg)) => {
                        if error_matches(class, detail, *kind) {
                            via = kind.map(|k| format!("{class}<-{}", k.as_str()));
                            (Status::Pass, via)
                        } else {
                            (
                                Status::Fail(format!(
                                    "expected {class} ({detail}), got error: {msg}"
                                )),
                                None,
                            )
                        }
                    }
                };
            }
        }
        let (cols, rows) = match res {
            Ok(r) => r,
            Err((_, msg)) => return (unexpected(msg, "query"), None),
        };
        if t == "the result should be empty" {
            if !rows.is_empty() {
                return (
                    Status::Fail(format!("expected empty result, got {} rows", rows.len())),
                    None,
                );
            }
            continue;
        }
        if t.starts_with("the result should be") {
            let ordered = t.contains("in order");
            let unordered_lists = t.contains("ignoring element order for lists");
            let Some(header) = step.table.first() else {
                return (
                    Status::Fail("harness: result table without header".into()),
                    None,
                );
            };
            if header != cols {
                return (
                    Status::Fail(format!("columns: expected {header:?}, got {cols:?}")),
                    None,
                );
            }
            let mut expected = Vec::new();
            for row in &step.table[1..] {
                let mut vals = Vec::new();
                for cell in row {
                    match parse_value(cell) {
                        Ok(v) => vals.push(v),
                        Err(e) => return (Status::Fail(format!("harness: {e}")), None),
                    }
                }
                expected.push(vals);
            }
            let row_eq = |a: &Vec<Out>, b: &Vec<TVal>| {
                a.len() == b.len()
                    && a.iter()
                        .zip(b)
                        .all(|(x, y)| matches_val(x, y, unordered_lists))
            };
            let ok = if ordered {
                rows.len() == expected.len()
                    && rows.iter().zip(&expected).all(|(a, b)| row_eq(a, b))
            } else {
                multiset_match(rows, &expected, row_eq)
            };
            if !ok {
                let shown: Vec<String> = rows
                    .iter()
                    .take(8)
                    .map(|r| r.iter().map(show_out).collect::<Vec<_>>().join(" | "))
                    .collect();
                return (
                    Status::Fail(format!(
                        "wrong result: expected {} rows {:?}, got {} rows [{}]",
                        expected.len(),
                        step.table[1..]
                            .iter()
                            .take(8)
                            .map(|r| r.join(" | "))
                            .collect::<Vec<_>>(),
                        rows.len(),
                        shown.join("; ")
                    )),
                    None,
                );
            }
            continue;
        }
        if t == "no side effects" || t.starts_with("the side effects should be") {
            let after = match ctx.snapshot() {
                Ok(s) => s,
                Err(e) => return (Status::Fail(format!("snapshot: {e}")), None),
            };
            let got = side_effects(before.as_ref().unwrap(), &after);
            let mut want: BTreeMap<&str, usize> = got.keys().map(|k| (*k, 0)).collect();
            for row in &step.table {
                if row.len() >= 2 {
                    let n = row[1].parse().unwrap_or(usize::MAX);
                    match want.get_mut(row[0].as_str()) {
                        Some(slot) => *slot = n,
                        None => {
                            return (
                                Status::Fail(format!("harness: side effect {}", row[0])),
                                None,
                            )
                        }
                    }
                }
            }
            if want != got {
                let diff: Vec<String> = want
                    .iter()
                    .filter(|(k, v)| got.get(*k) != Some(v))
                    .map(|(k, v)| format!("{k}: expected {v}, got {}", got[k]))
                    .collect();
                return (
                    Status::Fail(format!("side effects: {}", diff.join(", "))),
                    None,
                );
            }
            continue;
        }
        return (Status::Fail(format!("harness: unknown step: {t}")), None);
    }
    (Status::Pass, via)
}

fn features(dir: &Path, root: &Path, out: &mut Vec<(PathBuf, String)>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
    entries.sort_by_key(|e| e.path());
    for e in entries {
        let p = e.path();
        if p.is_dir() {
            features(&p, root, out);
        } else if p.extension().is_some_and(|x| x == "feature") {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
            out.push((p, rel));
        }
    }
}

/// "An empty graph" is an existing, empty namespace: reads on a namespace that was
/// never written are NOT_FOUND in ehgraph, and an empty upsert is refused. So write
/// one sentinel node and deactivate it: deactivated elements are invisible to MATCH.
fn create_namespace(graph: &EhGraph, ns: &str) {
    let created = graph.call(
        "tck",
        "graph.upsert",
        &json!({"namespace": ns,
                "nodes": [{"id": "__tck_sentinel__", "type": "__TckSentinel", "props": {}}],
                "source": "tck", "execution_id": "tck"}),
    );
    assert!(created.ok, "creating namespace {ns}: {created:?}");
    let gone = graph.call(
        "tck",
        "graph.deactivate",
        &json!({"namespace": ns, "ids": ["__tck_sentinel__"], "execution_id": "tck"}),
    );
    assert!(gone.ok, "sentinel {ns}: {gone:?}");
}

fn config(root: &Path) -> GraphConfig {
    GraphConfig::from_toml(&format!(
        r#"
instance_id = "ehgraph-tck"
domain = "tck"
data_dir = "{data}"
backup_prefix = "{backup}"
build = "tck"

[auth]
mode = "bearer"

[[auth.grants]]
token = "tck"
principal = "tck"
namespace = "*"
read = true
write = true
"#,
        data = root.join("data").display(),
        backup = root.join("backup").display(),
    ))
    .unwrap()
}

#[test]
#[ignore = "needs an openCypher checkout in EHGRAPH_TCK_DIR; run scripts/tck.sh"]
fn opencypher_tck() {
    let Ok(dir) = std::env::var("EHGRAPH_TCK_DIR") else {
        panic!("set EHGRAPH_TCK_DIR to an openCypher checkout (or its tck/ directory)");
    };
    let mut tck = PathBuf::from(dir);
    if tck.join("tck").is_dir() {
        tck = tck.join("tck");
    }
    let root = tck.join("features");
    let filter = std::env::var("EHGRAPH_TCK_FILTER").unwrap_or_default();
    let mut files = Vec::new();
    features(&root, &root, &mut files);

    let tmp = tempfile::tempdir().unwrap();
    let graph = EhGraph::open_standalone(config(tmp.path())).unwrap();
    let caller = graph.authorize("tck").unwrap();

    let mut all = Vec::new();
    let mut ignored = 0;
    for (path, rel) in &files {
        if !filter.is_empty() && !rel.contains(&filter) {
            continue;
        }
        for sc in parse_feature(path, rel) {
            if sc.tags.iter().any(|t| t == "@ignore") {
                ignored += 1;
            } else {
                all.push(sc);
            }
        }
    }

    // Scenarios are independent (one namespace each), so run them on a few threads:
    // the run is dominated by fsyncs of tiny commits.
    let threads: usize = std::env::var("EHGRAPH_TCK_THREADS")
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or(8);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<Verdict>>> =
        all.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Some(sc) = all.get(i) else { break };
                let mut ctx = Ctx {
                    graph: &graph,
                    caller: caller.clone(),
                    ns: format!("tck{}", i + 1),
                    params: serde_json::Map::new(),
                };
                let r = match static_oom(sc) {
                    Some(reason) => (Status::Oom(reason), None),
                    None => {
                        create_namespace(&graph, &ctx.ns);
                        run_scenario(&mut ctx, sc, &tck)
                    }
                };
                *results[i].lock().unwrap() = Some(r);
            });
        }
    });

    let mut scenarios_json = Vec::new();
    let (mut pass, mut fail, mut oom) = (0, 0, 0);
    for (sc, slot) in all.iter().zip(results) {
        let (status, via) = slot.into_inner().unwrap().expect("every scenario ran");
        let (s, reason) = match &status {
            Status::Pass => {
                pass += 1;
                ("pass", via.unwrap_or_default())
            }
            Status::Fail(r) => {
                fail += 1;
                if uses_id_property(sc) {
                    ("fail", format!("[uses `id` property] {r}"))
                } else {
                    ("fail", r.clone())
                }
            }
            Status::Oom(r) => {
                oom += 1;
                ("out-of-model", r.clone())
            }
        };
        let mut reason = reason;
        if reason.len() > 400 {
            let mut cut = 400;
            while !reason.is_char_boundary(cut) {
                cut -= 1;
            }
            reason.truncate(cut);
            reason.push('…');
        }
        scenarios_json.push(json!({
            "file": sc.file,
            "feature": sc.feature,
            "scenario": sc.name,
            "status": s,
            "reason": reason,
        }));
    }
    let total = pass + fail + oom;
    let pct = |a: usize, b: usize| {
        if b == 0 {
            0.0
        } else {
            (a as f64) * 100.0 / (b as f64)
        }
    };
    println!(
        "TCK: {total} scenarios: {pass} pass, {fail} fail, {oom} out-of-model ({ignored} @ignore not run)"
    );
    println!(
        "pass rate: {:.1}% of all, {:.1}% of in-model",
        pct(pass, total),
        pct(pass, pass + fail)
    );
    let report = json!({
        "totals": {"scenarios": total, "pass": pass, "fail": fail, "out_of_model": oom,
                   "ignored": ignored},
        "scenarios": scenarios_json,
    });
    let out = std::env::var("EHGRAPH_TCK_OUT").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/tck-results.json").into()
    });
    std::fs::write(&out, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    println!("report: {out}");
}

#[test]
fn tck_value_syntax() {
    let v = parse_value("<(:A {name: 'a'})-[:T {k: 1.5}]->(:B)<-[:S]-()>").unwrap();
    match v {
        TVal::Path { nodes, rels } => {
            assert_eq!(nodes.len(), 3);
            assert!(rels[0].1 && !rels[1].1);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        parse_value("[1, 'x\\'y', null, {a: [true]}]").unwrap(),
        TVal::List(vec![
            TVal::Int(1),
            TVal::Str("x'y".into()),
            TVal::Null,
            TVal::Map([("a".to_string(), TVal::List(vec![TVal::Bool(true)]))].into()),
        ])
    );
    assert_eq!(parse_value("-0.5").unwrap(), TVal::Float(-0.5));
    assert!(has_multi_label("CREATE (:A:B)"));
    assert!(has_multi_label("MATCH (n:A|B) RETURN n"));
    assert!(!has_multi_label("MATCH (a:A)-[r:T|S]->(b:B) RETURN a"));
    assert!(!has_multi_label("RETURN {a: 1, b: 2}"));
    assert!(has_id_property("CREATE ({id: 1})"));
    assert!(has_id_property("RETURN n.id"));
    assert!(!has_id_property("RETURN id(n)"));
    assert!(has_call_procedure("CALL test.labels()"));
    assert!(!has_call_procedure("CALL { MATCH (n) RETURN n }"));
    assert_eq!(split_row("| 'a\\|b' | 1 |"), vec!["'a|b'", "1"]);
}
