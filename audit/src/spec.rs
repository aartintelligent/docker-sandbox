//! `spec` — infer an OpenAPI 3.1 contract and a Markdown reference for the
//! `sandboxd` API from observed traffic.
//!
//! Inputs:
//!
//! 1. Router patterns, taken from the daemon log (the `route` field of
//!    `http request` entries, which carries the exact parameter names) and from
//!    the binary route inventory confirmed with `OPTIONS` (the `Allow` header
//!    lists the methods served on each route).
//! 2. Exchanges captured by the spy proxy: `request` and `response` events
//!    paired by `(conn, seq)`, each labelled with the CLI scenario (`tag`) that
//!    produced it, which tells which CLI command calls which route.
//! 3. Optionally, a `GET /events` stream recorded separately, and hand-written
//!    notes inserted verbatim into `API.md`.
//!
//! Each exchange is matched to the most specific router pattern and aggregated
//! per operation (pattern and method). Schemas are INFERRED from the samples
//! only: a field that never appears in a capture is absent from the schema, and
//! a field is `required` only when it is present in every observed sample. In
//! request bodies, that list is reported as `x-cli-always-sends` instead,
//! because what the CLI always sends is not necessarily what the daemon
//! requires. Objects whose keys look like caller-chosen data (environment
//! variables, labels, annotations, ...) are modelled as maps
//! (`additionalProperties`) rather than as fixed properties.
//!
//! Outputs, written to the output directory:
//!
//! - `openapi.json`: the OpenAPI 3.1 contract, annotated with `x-*` extensions
//!   describing what was observed (sample count, CLI scenarios, request
//!   headers, streaming, redacted values);
//! - `API.md`: a readable reference with the operations table, the call
//!   sequence of each CLI scenario, the event types and examples.

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::{self, Write as _};
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// Background requests the CLI sends before almost every command; they are
/// left out of the call sequences.
const NOISE: &[(&str, &str)] = &[
    ("GET", "/daemon/health"),
    ("GET", "/daemon/settings"),
    ("GET", "/policy/network/setup"),
];

// ---------------------------------------------------------------- route patterns

/// A router pattern, such as `/sandbox/:name/exec/:exec_id`, with the methods
/// known to be served on it.
#[derive(Debug, Clone)]
pub(crate) struct Pattern {
    /// Pattern as spelled by the router, e.g. `/sandbox/:name/exec/:exec_id`.
    pub(crate) raw: String,
    /// Path segments, e.g. `["sandbox", ":name", "exec", ":exec_id"]`.
    segs: Vec<String>,
    /// Methods confirmed by the daemon log or by an `Allow` header, `OPTIONS`
    /// excluded.
    methods: BTreeSet<String>,
    /// Whether the pattern comes from the daemon log, whose parameter names are
    /// authoritative.
    from_log: bool,
}

impl Pattern {
    /// Parses a raw pattern; no method is known yet.
    fn new(raw: &str) -> Self {
        let segs = raw
            .trim_start_matches('/')
            .split('/')
            .map(String::from)
            .collect();
        Self {
            raw: raw.to_string(),
            segs,
            methods: BTreeSet::new(),
            from_log: false,
        }
    }

    /// Returns the pattern with every parameter replaced by `*`, e.g.
    /// `sandbox/*/exec/*`. Different spellings of the same route share a shape.
    fn shape(&self) -> String {
        self.segs
            .iter()
            .map(|s| if s.starts_with(':') { "*" } else { s.as_str() })
            .collect::<Vec<_>>()
            .join("/")
    }

    /// Number of literal (non-parameter) segments, used to rank competing
    /// matches.
    fn literal_count(&self) -> usize {
        self.segs.iter().filter(|s| !s.starts_with(':')).count()
    }

    /// Whether `path` (without query string) matches the pattern: same number
    /// of segments, each one equal to the literal segment or, for a parameter,
    /// non-empty.
    fn matches(&self, path: &str) -> bool {
        let p: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        p.len() == self.segs.len()
            && self
                .segs
                .iter()
                .zip(&p)
                .all(|(s, x)| s.starts_with(':') && !x.is_empty() || s == x)
    }

    /// Returns the pattern in OpenAPI syntax, e.g.
    /// `/sandbox/{name}/exec/{exec_id}`.
    pub(crate) fn openapi_path(&self) -> String {
        let s: Vec<String> = self
            .segs
            .iter()
            .map(|s| {
                if let Some(n) = s.strip_prefix(':') {
                    format!("{{{n}}}")
                } else {
                    s.clone()
                }
            })
            .collect();
        format!("/{}", s.join("/"))
    }

    /// Names of the path parameters, in order of appearance.
    fn params(&self) -> Vec<String> {
        self.segs
            .iter()
            .filter_map(|s| s.strip_prefix(':').map(String::from))
            .collect()
    }
}

/// Loads the router patterns from the router log and the binary route
/// inventory.
///
/// Both sources are optional, and a missing file is ignored. Both are JSON
/// objects of the form `{"routes": {"<pattern>": "<methods>"}}`: the router log
/// (see [`router_log_routes`]) lists the patterns the daemon matched, with the
/// methods it served; the inventory lists the value of the `Allow` header.
///
/// Patterns are deduplicated by shape. When two spellings collide, the router
/// log wins because it carries the exact parameter names; between spellings
/// from the same source, the shorter one wins. Methods from both sources are
/// merged, except `OPTIONS`.
pub(crate) fn load_patterns(
    routes_json: Option<&Path>,
    router_log: Option<&Path>,
) -> Result<Vec<Pattern>> {
    // Keyed by shape (parameters replaced by `*`). The router log is
    // authoritative for parameter names.
    let mut by_shape: BTreeMap<String, Pattern> = BTreeMap::new();
    let mut add = |raw: &str, methods: &[String], from_log: bool| {
        let p = Pattern::new(raw);
        let e = by_shape.entry(p.shape()).or_insert_with(|| p.clone());
        // Within one source, prefer the shorter spelling, e.g. `:exec_id` over
        // `:exec_iddecode` (over-long names come from text extracted from the
        // binary).
        let better =
            (from_log && !e.from_log) || (from_log == e.from_log && p.raw.len() < e.raw.len());
        if better {
            let m = std::mem::take(&mut e.methods);
            *e = p;
            e.methods = m;
            e.from_log |= from_log;
        }
        for m in methods {
            if m != "OPTIONS" {
                e.methods.insert(m.clone());
            }
        }
    };
    for (file, from_log) in [(router_log, true), (routes_json, false)] {
        let Some(file) = file.filter(|p| p.exists()) else {
            continue;
        };
        let v: Value = serde_json::from_slice(&std::fs::read(file)?)
            .with_context(|| file.display().to_string())?;
        if let Some(obj) = v["routes"].as_object() {
            for (raw, allow) in obj {
                let methods: Vec<String> = allow
                    .as_str()
                    .unwrap_or("")
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                add(raw, &methods, from_log);
            }
        }
    }
    Ok(by_shape.into_values().collect())
}

/// The router patterns of the daemon log, from byte `offset` on: pattern →
/// methods the daemon served on it.
///
/// The log is read as JSON lines; only `http request` entries with a `method`
/// and a `route` are used. Requests that matched no route are logged with an
/// empty route and ignored. A 405 is logged with the route it matched although
/// the method was rejected: the pattern is kept, not the method. A 404 on a
/// matched route means a missing resource, so the method is real.
///
/// # Errors
///
/// Fails if the log cannot be read.
pub(crate) fn router_log_routes(
    log: &Path,
    offset: u64,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(log).with_context(|| log.display().to_string())?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut routes = BTreeMap::new();
    router_patterns(&String::from_utf8_lossy(&bytes), &mut routes);
    Ok(routes)
}

/// Adds the router patterns of daemon log `text` to `routes` (see
/// [`router_log_routes`]).
pub(crate) fn router_patterns(text: &str, routes: &mut BTreeMap<String, BTreeSet<String>>) {
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["msg"] != "http request" {
            continue;
        }
        let route = v["route"].as_str().filter(|r| !r.is_empty());
        if let (Some(method), Some(route)) = (v["method"].as_str(), route) {
            let methods = routes.entry(route.to_owned()).or_default();
            if v["status"].as_u64().is_none_or(|s| s != 405) {
                methods.insert(method.to_owned());
            }
        }
    }
}

/// Returns the most specific pattern matching `path`, i.e. the one with the
/// most literal segments, or `None` when no pattern matches.
pub(crate) fn match_pattern<'a>(patterns: &'a [Pattern], path: &str) -> Option<&'a Pattern> {
    patterns
        .iter()
        .filter(|pattern| pattern.matches(path))
        .max_by_key(|pattern| pattern.literal_count())
}

// ---------------------------------------------------------------- schema inference

/// Property names whose value is a free-form dictionary (keys chosen by the
/// caller), modelled as a map whatever their keys look like.
const MAP_KEYS: &[&str] = &["env", "environment", "labels", "annotations", "headers"];

/// Heuristic telling whether the keys of `m` are data rather than field names:
/// every key either contains `.`, `/` or `:` (dotted label keys, paths, URLs),
/// or is an upper-case identifier such as an environment variable name.
fn map_like_keys(m: &Map<String, Value>) -> bool {
    !m.is_empty()
        && m.keys().all(|k| {
            k.contains('.')
                || k.contains('/')
                || k.contains(':')
                || (k
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                    && k.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
        })
}

/// Converts an object schema into a map schema whose `additionalProperties`
/// merges the schemas of all known properties and any existing
/// `additionalProperties`.
fn as_map_schema(s: &Value) -> Value {
    let vals = s["properties"]
        .as_object()
        .map(|m| m.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let mut item = vals
        .into_iter()
        .reduce(|a, b| merge(&a, &b))
        .unwrap_or(json!({}));
    if let Some(a) = s.get("additionalProperties") {
        item = merge(&item, a);
    }
    json!({"type": "object", "additionalProperties": item})
}

/// Final pass: recursively turns the objects stored under one of the
/// `MAP_KEYS` names into maps.
fn mapify(v: &mut Value) {
    if let Some(props) = v.get_mut("properties").and_then(Value::as_object_mut) {
        for (k, sub) in props.iter_mut() {
            if MAP_KEYS.contains(&k.as_str()) && sub.get("properties").is_some() {
                *sub = as_map_schema(sub);
            }
            mapify(sub);
        }
    }
    for key in ["items", "additionalProperties"] {
        if let Some(sub) = v.get_mut(key) {
            mapify(sub);
        }
    }
    if let Some(xs) = v.get_mut("anyOf").and_then(Value::as_array_mut) {
        xs.iter_mut().for_each(mapify);
    }
}

/// Recursively renames every `required` list to `x-cli-always-sends`.
///
/// Applied to request bodies: "always sent by the CLI" does not mean
/// "required by the daemon".
fn soften_required(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    if let Some(required) = object.remove("required") {
        object.insert("x-cli-always-sends".into(), required);
    }
    // Recurse into sub-schemas only: a property literally named `required`
    // is a key of `properties`, not a schema keyword.
    if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
        properties.values_mut().for_each(soften_required);
    }
    for keyword in ["items", "additionalProperties", "x-ndjson-item"] {
        if let Some(sub) = object.get_mut(keyword) {
            soften_required(sub);
        }
    }
    if let Some(alternatives) = object.get_mut("anyOf").and_then(Value::as_array_mut) {
        alternatives.iter_mut().for_each(soften_required);
    }
}

/// Infers a JSON schema from a single sample.
///
/// Integers and floating-point numbers are told apart, strings redacted by the
/// spy (`<redacted:N>`) are flagged with `x-redacted-in-capture`, arrays get
/// the merged schema of their items, and objects list every key as `required`
/// (the intersection taken by `merge` relaxes this across samples). Objects
/// whose keys look like data (see `map_like_keys`) become maps.
fn infer(v: &Value) -> Value {
    match v {
        Value::Null => json!({"type": "null"}),
        Value::Bool(_) => json!({"type": "boolean"}),
        Value::Number(n) => json!({"type": if n.is_f64() { "number" } else { "integer" }}),
        Value::String(s) if s.starts_with("<redacted:") => {
            json!({"type": "string", "x-redacted-in-capture": true})
        }
        Value::String(_) => json!({"type": "string"}),
        Value::Array(a) => {
            let items = a
                .iter()
                .map(infer)
                .reduce(|x, y| merge(&x, &y))
                .unwrap_or(json!({}));
            json!({"type": "array", "items": items})
        }
        Value::Object(m) if map_like_keys(m) => {
            let item = m
                .values()
                .map(infer)
                .reduce(|a, b| merge(&a, &b))
                .unwrap_or(json!({}));
            json!({"type": "object", "additionalProperties": item})
        }
        Value::Object(m) => {
            let props: Map<String, Value> = m.iter().map(|(k, x)| (k.clone(), infer(x))).collect();
            let req: Vec<Value> = m.keys().map(|k| json!(k)).collect();
            json!({"type": "object", "properties": props, "required": req})
        }
    }
}

/// Returns the JSON types declared by the schema's `type`, which may be a
/// string or an array.
fn types_of(s: &Value) -> BTreeSet<String> {
    match &s["type"] {
        Value::String(t) => [t.clone()].into(),
        Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        _ => BTreeSet::new(),
    }
}

/// Sets the schema's `type` to `t`: a string when there is a single type, an
/// array otherwise.
fn with_types(mut s: Value, t: &BTreeSet<String>) -> Value {
    s["type"] = if let (1, Some(only)) = (t.len(), t.first()) {
        json!(only)
    } else {
        json!(t.iter().collect::<Vec<_>>())
    };
    s
}

/// Component name of an event schema: `Event_<type>_<action>`, restricted to
/// characters valid in OpenAPI component keys.
fn event_component(event_type: &str, action: &str) -> String {
    let clean = |s: &str| -> String {
        let cleaned: String = s
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        if cleaned.is_empty() {
            "none".to_owned()
        } else {
            cleaned
        }
    };
    format!("Event_{}_{}", clean(event_type), clean(action))
}

/// Combines two schemas as `anyOf` alternatives, flattening nested `anyOf`
/// lists and removing duplicates.
fn any_of(a: &Value, b: &Value) -> Value {
    let mut alternatives: Vec<Value> = Vec::new();
    for schema in [a, b] {
        let candidates = schema
            .get("anyOf")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_else(|| vec![schema.clone()]);
        for candidate in candidates {
            if !alternatives.contains(&candidate) {
                alternatives.push(candidate);
            }
        }
    }
    match alternatives.len() {
        1 => alternatives.remove(0),
        _ => json!({ "anyOf": alternatives }),
    }
}

/// Merges two inferred schemas into one that accepts both.
///
/// The empty schema `{}` is neutral. Object properties are united and their
/// `required` lists intersected, array item schemas are merged, `null` makes
/// the result nullable, `integer` widens to `number`, and distinct scalar
/// types are combined into a type list. Incompatible structured types fall
/// back to `anyOf`.
fn merge(a: &Value, b: &Value) -> Value {
    if a == b {
        return a.clone();
    }
    if a.as_object().is_none_or(Map::is_empty) {
        return b.clone();
    }
    if b.as_object().is_none_or(Map::is_empty) {
        return a.clone();
    }
    if a.get("anyOf").is_some() || b.get("anyOf").is_some() {
        return any_of(a, b);
    }
    let (ta, tb) = (types_of(a), types_of(b));
    let strip_null = |t: &BTreeSet<String>| {
        t.iter()
            .filter(|x| *x != "null")
            .cloned()
            .collect::<BTreeSet<_>>()
    };
    let (ca, cb) = (strip_null(&ta), strip_null(&tb));
    let nullable = ta.contains("null") || tb.contains("null");
    let mut all: BTreeSet<String> = ca.union(&cb).cloned().collect();
    if all.contains("number") {
        all.remove("integer");
    }
    // Expected inputs: a single "structured" type on each side, or one side
    // that is only `null`.
    let base = if ca.is_empty() {
        b.clone()
    } else if cb.is_empty() {
        a.clone()
    } else if ca == cb
        && ca.contains("object")
        && (a.get("additionalProperties").is_some() || b.get("additionalProperties").is_some())
    {
        let (ma, mb) = (as_map_schema(a), as_map_schema(b));
        json!({"type": "object", "additionalProperties": merge(&ma["additionalProperties"], &mb["additionalProperties"])})
    } else if ca == cb && ca.contains("object") {
        let pa = a["properties"].as_object().cloned().unwrap_or_default();
        let pb = b["properties"].as_object().cloned().unwrap_or_default();
        let mut props = Map::new();
        for k in pa.keys().chain(pb.keys()) {
            let v = match (pa.get(k), pb.get(k)) {
                (Some(x), Some(y)) => merge(x, y),
                (Some(x), None) | (None, Some(x)) => x.clone(),
                _ => unreachable!(),
            };
            props.insert(k.clone(), v);
        }
        let ra: BTreeSet<String> = a["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
        let rb: BTreeSet<String> = b["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
        let req: Vec<&String> = ra.intersection(&rb).collect();
        json!({"type": "object", "properties": props, "required": req})
    } else if ca == cb && ca.contains("array") {
        json!({"type": "array", "items": merge(&a["items"], &b["items"])})
    } else if !all.contains("object") && !all.contains("array") {
        with_types(json!({}), &all)
    } else {
        return any_of(a, b);
    };
    if nullable {
        let mut t = types_of(&base);
        t.insert("null".into());
        with_types(base, &t)
    } else {
        base
    }
}

/// Shortens a sample for use as an example: strings longer than 200
/// characters are truncated, arrays are cut to 3 items followed by a marker,
/// and objects nested deeper than 6 levels are replaced by a placeholder.
fn shorten(v: &Value, depth: usize) -> Value {
    match v {
        Value::String(s) if s.chars().count() > 200 => {
            json!(format!("{}…", s.chars().take(200).collect::<String>()))
        }
        Value::Array(a) => {
            let mut out: Vec<Value> = a.iter().take(3).map(|x| shorten(x, depth + 1)).collect();
            if a.len() > 3 {
                out.push(json!(format!("… +{} more items", a.len() - 3)));
            }
            json!(out)
        }
        Value::Object(m) if depth > 6 => json!(format!("{{… {} fields}}", m.len())),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), shorten(x, depth + 1)))
                .collect(),
        ),
        _ => v.clone(),
    }
}

// ---------------------------------------------------------------- captured exchanges

/// One captured request, with its response when one was captured.
///
/// Bodies are stored as encoded by the spy: a JSON value, a plain string, an
/// `{"ndjson": [...]}` envelope for line-delimited JSON, or a
/// `{"binary_preview": "..."}` envelope for binary data.
#[derive(Default)]
pub(crate) struct Exchange {
    /// CLI scenario label set by the spy; empty when the request is unlabelled.
    pub(crate) tag: String,
    /// HTTP method.
    pub(crate) method: String,
    /// Request path, without the query string.
    pub(crate) path: String,
    /// Percent-decoded query parameters, in request order.
    pub(crate) query: Vec<(String, String)>,
    /// Request headers, as captured.
    pub(crate) req_headers: Map<String, Value>,
    /// Request body; `None` when absent or empty.
    pub(crate) req_body: Option<Value>,
    /// Response status; `None` when no response was captured.
    pub(crate) status: Option<u16>,
    /// Response headers, as captured.
    pub(crate) resp_headers: Map<String, Value>,
    /// Response body; `None` when absent, empty or not captured.
    pub(crate) resp_body: Option<Value>,
}

/// Decodes a percent-encoded query component, where `+` stands for a space.
/// Invalid escape sequences are kept as is, and invalid UTF-8 is replaced
/// with U+FFFD.
fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| {
                char::from(c)
                    .to_digit(16)
                    .and_then(|d| u8::try_from(d).ok())
            };
            if let (Some(high), Some(low)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(high * 16 + low);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Loads the exchanges recorded in spy capture files (JSON lines).
///
/// Each `request` event starts an exchange, and the `response` event with the
/// same `(conn, seq)` in the same file completes it. Other event kinds and
/// lines that are not valid JSON are skipped. Exchanges are returned in
/// capture order.
pub(crate) fn load_exchanges(captures: &[PathBuf]) -> Result<Vec<Exchange>> {
    let mut exchanges: Vec<Exchange> = vec![];
    for capture in captures {
        // Connection ids restart with every capture file.
        let mut pending: HashMap<(u64, u64), usize> = HashMap::new();
        let file = std::fs::File::open(capture).with_context(|| capture.display().to_string())?;
        for line in std::io::BufReader::new(file).lines() {
            let Ok(event) = serde_json::from_str::<Value>(&line?) else {
                continue;
            };
            let key = (
                event["conn"].as_u64().unwrap_or(0),
                event["seq"].as_u64().unwrap_or(0),
            );
            let body = || {
                event
                    .get("body")
                    .filter(|b| !b.is_null() && *b != "")
                    .cloned()
            };
            let headers = || event["headers"].as_object().cloned().unwrap_or_default();
            match event["kind"].as_str() {
                Some("request") => {
                    let target = event["path"].as_str().unwrap_or("/");
                    let (path, query) = target.split_once('?').unwrap_or((target, ""));
                    let query = query
                        .split('&')
                        .filter(|pair| !pair.is_empty())
                        .map(|pair| {
                            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                            (pct_decode(name), pct_decode(value))
                        })
                        .collect();
                    pending.insert(key, exchanges.len());
                    exchanges.push(Exchange {
                        tag: event["tag"].as_str().unwrap_or("").to_string(),
                        method: event["method"].as_str().unwrap_or("GET").to_string(),
                        path: path.to_string(),
                        query,
                        req_headers: headers(),
                        req_body: body(),
                        ..Default::default()
                    });
                }
                Some("response") => {
                    if let Some(&index) = pending.get(&key) {
                        let exchange = &mut exchanges[index];
                        exchange.status =
                            event["status"].as_u64().and_then(|s| u16::try_from(s).ok());
                        exchange.resp_headers = headers();
                        exchange.resp_body = body();
                    }
                }
                _ => {}
            }
        }
    }
    Ok(exchanges)
}

/// Looks up a header by name, case-insensitively. Non-string values are
/// ignored.
fn header<'a>(h: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    h.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| v.as_str())
}

/// Returns `(content type, schema, example)` for a captured body.
///
/// NDJSON bodies are described as a string whose item schema is given in
/// `x-ndjson-item`. Binary bodies (a spy `binary_preview`, or
/// `application/x-tar`) are described as binary strings without example.
/// Other strings are text, and any other value is JSON. The declared
/// `Content-Type`, without its parameters, takes precedence over these
/// defaults, except for NDJSON.
fn body_media(body: &Value, ctype: Option<&str>) -> (String, Value, Value) {
    let ct = ctype.map(|c| c.split(';').next().unwrap_or(c).trim().to_string());
    if let Some(items) = body.get("ndjson").and_then(Value::as_array) {
        let item = items
            .iter()
            .map(infer)
            .reduce(|a, b| merge(&a, &b))
            .unwrap_or(json!({}));
        let ex = json!(
            items
                .iter()
                .take(3)
                .map(|x| shorten(x, 0))
                .collect::<Vec<_>>()
        );
        return (
            "application/x-ndjson".into(),
            json!({"type": "string", "x-ndjson-item": item}),
            ex,
        );
    }
    if body.get("binary_preview").is_some() || ct.as_deref() == Some("application/x-tar") {
        return (
            ct.unwrap_or("application/octet-stream".into()),
            json!({"type": "string", "format": "binary"}),
            Value::Null,
        );
    }
    match body {
        Value::String(s) => (
            ct.unwrap_or("text/plain".into()),
            json!({"type": "string"}),
            json!(shorten(&json!(s), 0)),
        ),
        v => (
            ct.unwrap_or("application/json".into()),
            infer(v),
            shorten(v, 0),
        ),
    }
}

// ---------------------------------------------------------------- per-operation aggregation

/// Everything observed for one operation (router pattern and method).
#[derive(Default)]
struct OpAgg {
    /// Number of captured exchanges.
    count: u32,
    /// CLI scenarios that called the operation.
    tags: BTreeSet<String>,
    /// Query parameter name → distinct values seen (truncated to 80 characters).
    query: BTreeMap<String, BTreeSet<String>>,
    /// Notable request header (`Accept`, `Content-Type`, `Upgrade`,
    /// `Connection`) → distinct values seen.
    req_headers: BTreeMap<String, BTreeSet<String>>,
    /// Request content type → (merged schema, first example).
    req: BTreeMap<String, (Value, Value)>,
    /// Response status → content type → (merged schema, first example).
    resp: BTreeMap<u16, BTreeMap<String, (Value, Value)>>,
    /// Statuses observed with an empty response body.
    resp_empty: BTreeSet<u16>,
    /// Response status → non-standard header (e.g. `Sandboxes-Exec-Id`) →
    /// first value seen.
    resp_headers: BTreeMap<u16, BTreeMap<String, String>>,
    /// Number of requests whose response was never captured.
    no_response: u32,
}

/// Standard response headers (lower case), not worth documenting per
/// operation.
const STD_HEADERS: &[&str] = &[
    "content-type",
    "content-length",
    "date",
    "transfer-encoding",
    "connection",
    "upgrade",
    "allow",
];

/// Adds a body sample to a content-type map: merges its schema into the
/// existing one and keeps the first non-null example.
fn add_media(map: &mut BTreeMap<String, (Value, Value)>, ct: String, schema: Value, ex: Value) {
    match map.get_mut(&ct) {
        Some((s, e)) => {
            *s = merge(s, &schema);
            if e.is_null() {
                *e = ex;
            }
        }
        None => {
            map.insert(ct, (schema, ex));
        }
    }
}

impl OpAgg {
    /// Adds one captured exchange to the aggregate.
    fn record(&mut self, exchange: &Exchange) {
        self.count += 1;
        if !exchange.tag.is_empty() {
            self.tags.insert(exchange.tag.clone());
        }
        for (name, value) in &exchange.query {
            self.query
                .entry(name.clone())
                .or_default()
                .insert(value.chars().take(80).collect());
        }
        for name in ["Accept", "Content-Type", "Upgrade", "Connection"] {
            if let Some(value) = header(&exchange.req_headers, name)
                && !(name == "Connection" && value.eq_ignore_ascii_case("close"))
            {
                self.req_headers
                    .entry(name.to_owned())
                    .or_default()
                    .insert(value.to_owned());
            }
        }
        if let Some(body) = &exchange.req_body {
            let (content_type, schema, example) =
                body_media(body, header(&exchange.req_headers, "Content-Type"));
            add_media(&mut self.req, content_type, schema, example);
        }
        if let Some(status) = exchange.status {
            for (name, value) in &exchange.resp_headers {
                if !STD_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
                    self.resp_headers
                        .entry(status)
                        .or_default()
                        .entry(name.clone())
                        .or_insert_with(|| value.as_str().unwrap_or("").chars().take(80).collect());
                }
            }
        }
        match (exchange.status, &exchange.resp_body) {
            (None, _) => self.no_response += 1,
            (Some(status), Some(body)) => {
                let (content_type, schema, example) =
                    body_media(body, header(&exchange.resp_headers, "Content-Type"));
                add_media(
                    self.resp.entry(status).or_default(),
                    content_type,
                    schema,
                    example,
                );
            }
            (Some(status), None) => {
                self.resp_empty.insert(status);
            }
        }
    }

    /// Every status observed, with or without a response body.
    fn statuses(&self) -> BTreeSet<u16> {
        let mut statuses: BTreeSet<u16> = self.resp.keys().copied().collect();
        statuses.extend(&self.resp_empty);
        statuses
    }
}

/// Options of the `spec` command.
pub(crate) struct SpecOpts<'a> {
    /// Capture files (`capture.jsonl`) written by the spy.
    pub captures: &'a [PathBuf],
    /// Route inventory: the binary route probe, confirmed with `OPTIONS`.
    pub routes: Option<&'a Path>,
    /// Router patterns extracted from the daemon log (see
    /// [`router_log_routes`]), the source of the exact parameter names.
    pub router_log: Option<&'a Path>,
    /// `GET /events` stream recorded during the audit (raw NDJSON).
    pub events: Option<&'a Path>,
    /// Hand-written notes (behaviors that cannot be inferred), inserted
    /// verbatim into `API.md`.
    pub notes: Option<&'a str>,
    /// Output directory for `openapi.json` and `API.md`.
    pub out: &'a Path,
}

// ---------------------------------------------------------------- event stream

/// The events of one `(type, action)` pair in a recorded event stream.
struct EventGroup {
    /// Number of events.
    count: u32,
    /// Schema merged over all the events.
    schema: Value,
    /// First event, shortened.
    example: Value,
}

/// Recorded events grouped by `(type, action)`.
type EventGroups = BTreeMap<(String, String), EventGroup>;

/// Reads a `GET /events` stream recorded as raw NDJSON. Lines that are not
/// valid JSON are skipped.
fn read_event_stream(path: &Path) -> Result<Vec<Value>> {
    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str(line.trim()).ok())
        .collect())
}

/// Groups recorded events by `(type, action)`.
fn group_events(events: &[Value]) -> EventGroups {
    let mut groups = EventGroups::new();
    for event in events {
        let key = (
            event["type"].as_str().unwrap_or("?").to_owned(),
            event["action"].as_str().unwrap_or("").to_owned(),
        );
        let group = groups.entry(key).or_insert_with(|| EventGroup {
            count: 0,
            schema: json!({}),
            example: Value::Null,
        });
        group.count += 1;
        group.schema = merge(&group.schema, &infer(event));
        if group.example.is_null() {
            group.example = shorten(event, 0);
        }
    }
    groups
}

/// Synthetic exchange documenting `GET /events`: the stream is recorded
/// outside the spy, so it is injected among the captured exchanges.
fn events_exchange(events: &[Value]) -> Exchange {
    let mut headers = Map::new();
    headers.insert("Content-Type".into(), json!("application/x-ndjson"));
    Exchange {
        tag: "events-stream".into(),
        method: "GET".into(),
        path: "/events".into(),
        status: Some(200),
        resp_headers: headers,
        resp_body: Some(json!({ "ndjson": events })),
        ..Default::default()
    }
}

// ---------------------------------------------------------------- aggregation

/// The captured exchanges, aggregated per operation.
#[derive(Default)]
struct Aggregate {
    /// Operations keyed by (raw pattern, method).
    ops: BTreeMap<(String, String), OpAgg>,
    /// Captured requests (`METHOD path`) that match no known pattern.
    unmatched: BTreeSet<String>,
    /// Call sequence of each scenario run, in capture order.
    sequences: Vec<(String, Vec<String>)>,
    /// Body of the first `GET /daemon/health` response.
    health: Option<Value>,
    /// Number of exchanges aggregated, `OPTIONS` requests included.
    exchanges: usize,
}

impl Aggregate {
    /// Matches each exchange to its most specific pattern and aggregates it.
    fn build(patterns: &[Pattern], exchanges: &[Exchange]) -> Self {
        let mut agg = Self {
            exchanges: exchanges.len(),
            ..Self::default()
        };
        for exchange in exchanges {
            // Router responses (`Allow` header), not part of the contract.
            if exchange.method == "OPTIONS" {
                continue;
            }
            let Some(pattern) = match_pattern(patterns, &exchange.path) else {
                agg.unmatched
                    .insert(format!("{} {}", exchange.method, exchange.path));
                continue;
            };
            agg.ops
                .entry((pattern.raw.clone(), exchange.method.clone()))
                .or_default()
                .record(exchange);
            if pattern.raw == "/daemon/health" && agg.health.is_none() {
                agg.health.clone_from(&exchange.resp_body);
            }
            agg.record_call(pattern, exchange);
        }
        agg
    }

    /// Appends the exchange to the call sequence of its scenario. Background
    /// requests are left out and consecutive duplicates collapsed.
    fn record_call(&mut self, pattern: &Pattern, exchange: &Exchange) {
        let noisy = NOISE
            .iter()
            .any(|(method, route)| *method == exchange.method && *route == pattern.raw);
        if noisy || exchange.tag.is_empty() {
            return;
        }
        let call = format!("{} {}", exchange.method, pattern.openapi_path());
        match self.sequences.last_mut() {
            Some((tag, calls)) if *tag == exchange.tag => {
                if calls.last() != Some(&call) {
                    calls.push(call);
                }
            }
            _ => self.sequences.push((exchange.tag.clone(), vec![call])),
        }
    }

    /// Methods to document for `pattern`: those known from the router plus
    /// those observed in the captures.
    fn methods(&self, pattern: &Pattern) -> BTreeSet<String> {
        let mut methods = pattern.methods.clone();
        methods.extend(
            self.ops
                .keys()
                .filter(|(raw, _)| *raw == pattern.raw)
                .map(|(_, method)| method.clone()),
        );
        methods
    }

    /// Aggregate of an operation, when it was observed.
    fn op(&self, pattern: &Pattern, method: &str) -> Option<&OpAgg> {
        self.ops.get(&(pattern.raw.clone(), method.to_owned()))
    }

    /// Number of documented operations, and of those observed.
    fn operation_counts(&self, patterns: &[Pattern]) -> (usize, usize) {
        patterns.iter().fold((0, 0), |(total, observed), pattern| {
            let methods = self.methods(pattern);
            let seen = methods
                .iter()
                .filter(|m| self.op(pattern, m).is_some())
                .count();
            (total + methods.len(), observed + seen)
        })
    }

    /// A string field of the `GET /daemon/health` response, or `unknown`.
    fn health_field(&self, name: &str) -> &str {
        self.health
            .as_ref()
            .and_then(|health| health[name].as_str())
            .unwrap_or("unknown")
    }

    /// Longest call sequence of each scenario, in order of first appearance.
    /// A scenario may have run several times, e.g. a failure, then a success.
    fn best_sequences(&self) -> Vec<(&str, &[String])> {
        let mut best: Vec<(&str, &[String])> = Vec::new();
        for (tag, calls) in &self.sequences {
            match best.iter_mut().find(|(t, _)| t == tag) {
                Some(entry) if entry.1.len() < calls.len() => entry.1 = calls,
                Some(_) => {}
                None => best.push((tag, calls)),
            }
        }
        best
    }
}

// ---------------------------------------------------------------- generation

/// Runs the `spec` command.
///
/// Loads the router patterns, the captured exchanges and the optional event
/// stream, aggregates the exchanges per operation, then writes `openapi.json`
/// and `API.md` to `opts.out` and prints a one-line summary on stderr.
///
/// # Errors
///
/// Fails when an input file cannot be read or parsed as expected, or when an
/// output file cannot be written.
pub(crate) fn run(opts: &SpecOpts<'_>) -> Result<SpecSummary> {
    let patterns = load_patterns(opts.routes, opts.router_log)?;
    let mut exchanges = load_exchanges(opts.captures)?;
    let events = match opts.events {
        Some(path) => {
            let events = read_event_stream(path)?;
            exchanges.push(events_exchange(&events));
            group_events(&events)
        }
        None => EventGroups::new(),
    };
    let agg = Aggregate::build(&patterns, &exchanges);

    std::fs::create_dir_all(opts.out)?;
    let openapi = openapi_document(&patterns, &agg, &events);
    std::fs::write(
        opts.out.join("openapi.json"),
        serde_json::to_vec_pretty(&openapi)?,
    )?;
    std::fs::write(
        opts.out.join("API.md"),
        api_markdown(opts, &patterns, &agg, &events)?,
    )?;

    let (total, observed) = agg.operation_counts(&patterns);
    eprintln!(
        "{total} operations ({observed} observed), {} exchanges, {} unmatched → {}/openapi.json, {}/API.md",
        agg.exchanges,
        agg.unmatched.len(),
        opts.out.display(),
        opts.out.display()
    );
    Ok(SpecSummary {
        operations: total,
        observed,
        exchanges: agg.exchanges,
        unmatched: agg.unmatched.len(),
    })
}

/// Counts reported by [`run`].
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub(crate) struct SpecSummary {
    /// Operations of the contract.
    pub operations: usize,
    /// Operations observed with at least one exchange.
    pub observed: usize,
    /// Exchanges read from the captures.
    pub exchanges: usize,
    /// Captured requests matching no router pattern.
    pub unmatched: usize,
}

/// Builds the OpenAPI 3.1 document.
fn openapi_document(patterns: &[Pattern], agg: &Aggregate, events: &EventGroups) -> Value {
    let mut paths = Map::new();
    for pattern in patterns {
        let methods = agg.methods(pattern);
        if methods.is_empty() {
            continue;
        }
        let item: Map<String, Value> = methods
            .iter()
            .map(|method| {
                let operation = operation_object(pattern, method, agg.op(pattern, method));
                (method.to_lowercase(), operation)
            })
            .collect();
        paths.insert(pattern.openapi_path(), Value::Object(item));
    }

    let version = agg.health_field("version");
    let mut doc = json!({
        "openapi": "3.1.0",
        "info": {
            "title": "sandboxd (Docker Sandboxes) — undocumented local API",
            "version": agg.health_field("api_version"),
            "description": format!("Contract INFERRED by audit from the traffic of the sbx CLI {version} and from the router inventory. \
                Transport: HTTP/1.1 over a Unix socket, without authentication. Schemas only contain the fields that were observed."),
        },
        "servers": [{"url": "http://sandboxd", "description": "Unix socket ~/.local/state/sandboxes/sandboxes/sandboxd/sandboxd.sock"}],
        "x-sbx-version": version,
        "paths": paths,
        "components": {"schemas": {"Error": {"type": "object",
            "properties": {"message": {"type": "string"}, "code": {"type": "string", "description": "Machine-readable code, present on some errors (e.g. invalid_input, exec_not_running)"}, "error": {"type": "string"}},
            "required": ["message"]}}},
    });

    if !events.is_empty() {
        for ((event_type, action), group) in events {
            doc["components"]["schemas"][event_component(event_type, action)] =
                group.schema.clone();
        }
        // Event types are shared by several actions, so `type` alone cannot
        // serve as a discriminator.
        let refs: Vec<Value> = events
            .keys()
            .map(|(event_type, action)| {
                json!({"$ref": format!("#/components/schemas/{}", event_component(event_type, action))})
            })
            .collect();
        let operation = doc
            .get_mut("paths")
            .and_then(|p| p.get_mut("/events"))
            .and_then(|p| p.get_mut("get"))
            .and_then(Value::as_object_mut);
        if let Some(operation) = operation {
            operation.insert("x-event-item".into(), json!({"oneOf": refs}));
            operation.insert("description".into(), json!("Endless ndjson stream. On connection: a replay of the recent state, then a `sync`/`complete` event, then live events."));
        }
    }
    doc
}

/// Builds the OpenAPI operation object of `method` on `pattern`.
fn operation_object(pattern: &Pattern, method: &str, observed: Option<&OpAgg>) -> Value {
    let mut params: Vec<Value> = pattern
        .params()
        .iter()
        .map(|name| json!({"name": name, "in": "path", "required": true, "schema": {"type": "string"}}))
        .collect();
    let mut op = Map::new();
    op.insert(
        "operationId".into(),
        json!(operation_id(method, &pattern.raw)),
    );
    op.insert("tags".into(), json!([domain(&pattern.raw)]));
    match observed {
        None => {
            op.insert(
                "summary".into(),
                json!("Route confirmed by OPTIONS but never observed: format unknown"),
            );
            op.insert("x-observed".into(), json!(false));
            op.insert(
                "responses".into(),
                json!({"default": {"description": "Not observed"}}),
            );
        }
        Some(agg) => {
            op.insert("x-observed".into(), json!(true));
            op.insert("x-sample-count".into(), json!(agg.count));
            op.insert("x-cli-scenarios".into(), json!(agg.tags));
            for (name, values) in &agg.query {
                params.push(json!({"name": name, "in": "query", "required": false,
                    "schema": {"type": "string"}, "example": values.iter().next()}));
            }
            if !agg.req_headers.is_empty() {
                op.insert("x-request-headers".into(), json!(agg.req_headers));
            }
            if !agg.req.is_empty() {
                let content: Map<String, Value> = agg
                    .req
                    .iter()
                    .map(|(content_type, (schema, example))| {
                        let mut media = media_obj(schema, example);
                        soften_required(&mut media["schema"]);
                        (content_type.clone(), media)
                    })
                    .collect();
                op.insert("requestBody".into(), json!({"content": content,
                    "description": "Schema inferred from CLI traffic. `x-cli-always-sends` lists the fields the CLI always sends; the daemon may accept fewer."}));
            }
            if agg.no_response > 0 {
                op.insert("x-streaming".into(), json!("Response never completed during capture (long-lived stream or hijacked connection)"));
            }
            op.insert("responses".into(), responses_object(agg));
        }
    }
    if !params.is_empty() {
        op.insert("parameters".into(), json!(params));
    }
    Value::Object(op)
}

/// Builds the OpenAPI responses object of an observed operation.
fn responses_object(agg: &OpAgg) -> Value {
    let mut responses = Map::new();
    for (status, media) in &agg.resp {
        let content: Map<String, Value> = media
            .iter()
            .map(|(content_type, (schema, example))| {
                (content_type.clone(), media_obj(schema, example))
            })
            .collect();
        responses.insert(
            status.to_string(),
            json!({"description": status_text(*status), "content": content}),
        );
    }
    for status in &agg.resp_empty {
        responses
            .entry(status.to_string())
            .or_insert(json!({"description": status_text(*status)}));
    }
    for (status, headers) in &agg.resp_headers {
        if let Some(response) = responses.get_mut(&status.to_string()) {
            let headers: Map<String, Value> = headers
                .iter()
                .map(|(name, example)| {
                    (
                        name.clone(),
                        json!({"schema": {"type": "string"}, "example": example}),
                    )
                })
                .collect();
            response["headers"] = Value::Object(headers);
        }
    }
    if responses.is_empty() {
        responses.insert(
            "default".into(),
            json!({"description": "No complete response captured"}),
        );
    }
    Value::Object(responses)
}

/// Renders `API.md`, the human-readable reference.
fn api_markdown(
    opts: &SpecOpts<'_>,
    patterns: &[Pattern],
    agg: &Aggregate,
    events: &EventGroups,
) -> Result<String> {
    let (total, observed) = agg.operation_counts(patterns);
    let sources: Vec<String> = opts
        .captures
        .iter()
        .map(|capture| {
            let name = capture.file_name().unwrap_or(capture.as_os_str());
            format!("`{}`", name.to_string_lossy())
        })
        .collect();
    let mut md = String::new();
    write!(
        md,
        "# sandboxd API — inferred reference\n\n- sbx: `{}` · api_version: `{}`\n- {total} operations, {observed} of them observed with examples\n- Machine-readable contract: `openapi.json` (OpenAPI 3.1)\n- Generated by `audit spec` from: {}\n\n",
        agg.health_field("version"),
        agg.health_field("api_version"),
        sources.join(", ")
    )?;
    md.push_str("## Transport\n\nHTTP/1.1 over a Unix socket (any `Host` value), no authentication: access is controlled by the socket file permissions (0600). Errors: JSON body `{\"message\": \"…\"}`. The CLI opens one connection per request (`Connection: close`).\n\n");
    if let Some(notes) = opts.notes {
        md.push_str(notes.trim_end());
        md.push_str("\n\n");
    }
    write_operations_table(&mut md, patterns, agg)?;
    write_call_sequences(&mut md, agg)?;
    write_events(&mut md, events)?;
    write_examples(&mut md, patterns, agg)?;
    if !agg.unmatched.is_empty() {
        md.push_str("## Captured requests matching no known pattern\n\n");
        for request in &agg.unmatched {
            writeln!(md, "- `{request}`")?;
        }
    }
    Ok(md)
}

/// Writes the table of every documented operation.
fn write_operations_table(md: &mut String, patterns: &[Pattern], agg: &Aggregate) -> fmt::Result {
    md.push_str("## Operations\n\n| Domain | Method | Route | Observed | Statuses | CLI scenarios |\n|---|---|---|---|---|---|\n");
    for pattern in patterns {
        for method in agg.methods(pattern) {
            let observed = agg.op(pattern, &method);
            let statuses = observed
                .map(|op| {
                    op.statuses()
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let scenarios = observed
                .map(|op| {
                    op.tags
                        .iter()
                        .take(6)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            writeln!(
                md,
                "| {} | {method} | `{}` | {} | {statuses} | {scenarios} |",
                domain(&pattern.raw),
                pattern.openapi_path(),
                if observed.is_some() { "yes" } else { "**no**" },
            )?;
        }
    }
    Ok(())
}

/// Writes the call sequence of each CLI scenario.
fn write_call_sequences(md: &mut String, agg: &Aggregate) -> fmt::Result {
    let noise: Vec<String> = NOISE
        .iter()
        .map(|(method, route)| format!("`{method} {route}`"))
        .collect();
    write!(
        md,
        "\n## Call sequences per CLI command\n\nBackground requests omitted: {}.\n\n",
        noise.join(", ")
    )?;
    for (tag, calls) in agg.best_sequences() {
        write!(md, "**{tag}**\n\n")?;
        for (index, call) in calls.iter().enumerate() {
            writeln!(md, "{}. `{call}`", index + 1)?;
        }
        md.push('\n');
    }
    Ok(())
}

/// Writes the event types of the recorded `GET /events` stream, if any.
fn write_events(md: &mut String, events: &EventGroups) -> fmt::Result {
    if events.is_empty() {
        return Ok(());
    }
    md.push_str("## Event stream `GET /events`\n\nEndless ndjson stream (`Transfer-Encoding: chunked`). On connection, the daemon replays the recent state, then sends a `sync`/`complete` event; live events follow. Schemas: `components.schemas.Event_*` in `openapi.json`.\n\n| type | action | seen | fields |\n|---|---|---|---|\n");
    for ((event_type, action), group) in events {
        let fields: Vec<String> = group.schema["properties"]
            .as_object()
            .map(|properties| properties.keys().cloned().collect())
            .unwrap_or_default();
        writeln!(
            md,
            "| `{event_type}` | `{action}` | {} | {} |",
            group.count,
            fields.join(", ")
        )?;
    }
    md.push('\n');
    for ((event_type, action), group) in events {
        write!(
            md,
            "`{event_type}` / `{action}`:\n\n```json\n{}\n```\n\n",
            clip(&group.example)
        )?;
    }
    Ok(())
}

/// Writes the captured examples of every observed operation.
fn write_examples(md: &mut String, patterns: &[Pattern], agg: &Aggregate) -> fmt::Result {
    md.push_str("## Examples\n\n");
    for ((raw, method), op) in &agg.ops {
        let Some(pattern) = patterns.iter().find(|p| p.raw == *raw) else {
            continue;
        };
        write!(md, "### `{method} {}`\n\n", pattern.openapi_path())?;
        if !op.query.is_empty() {
            let names: Vec<String> = op.query.keys().map(|name| format!("`{name}`")).collect();
            write!(md, "Query: {}\n\n", names.join(", "))?;
        }
        if !op.req_headers.is_empty() {
            let headers: Vec<String> = op
                .req_headers
                .iter()
                .map(|(name, values)| {
                    let values: Vec<&str> = values.iter().map(String::as_str).collect();
                    format!("`{name}: {}`", values.join(" | "))
                })
                .collect();
            write!(md, "Request headers: {}\n\n", headers.join(", "))?;
        }
        for (content_type, (_, example)) in &op.req {
            write!(
                md,
                "Request `{content_type}`:\n\n```json\n{}\n```\n\n",
                clip(example)
            )?;
        }
        for (status, media) in &op.resp {
            for (content_type, (_, example)) in media {
                write!(
                    md,
                    "Response {status} `{content_type}`:\n\n```json\n{}\n```\n\n",
                    clip(example)
                )?;
            }
        }
        for (status, headers) in &op.resp_headers {
            let names: Vec<String> = headers.keys().map(|name| format!("`{name}`")).collect();
            write!(md, "Response {status} headers: {}\n\n", names.join(", "))?;
        }
        if op.no_response > 0 {
            md.push_str("_Stream: the response did not complete during capture._\n\n");
        }
    }
    Ok(())
}

/// Builds an OpenAPI media type object: the schema after the `mapify` pass
/// (also applied to the `x-ndjson-item` schema, which `mapify` does not walk),
/// plus the example when there is one.
fn media_obj(schema: &Value, example: &Value) -> Value {
    let mut schema = schema.clone();
    mapify(&mut schema);
    if let Some(item) = schema.get_mut("x-ndjson-item") {
        mapify(item);
    }
    let mut m = json!({"schema": schema});
    if !example.is_null() {
        m["example"] = example.clone();
    }
    m
}

/// Pretty-prints a JSON example for `API.md`, truncated to 1500 characters.
fn clip(v: &Value) -> String {
    let s = serde_json::to_string_pretty(v).unwrap_or_default();
    if s.chars().count() > 1500 {
        format!(
            "{}\n… (truncated)",
            s.chars().take(1500).collect::<String>()
        )
    } else {
        s
    }
}

/// Returns the domain of a route, used as its OpenAPI tag: the first path
/// segment, with the exec, MCP and port routes split out of `sandbox`.
fn domain(route: &str) -> String {
    let segs: Vec<&str> = route.trim_start_matches('/').split('/').collect();
    match segs.as_slice() {
        ["sandbox", ":name", "exec", ..] => "exec".into(),
        ["sandbox", ":name", "mcp", ..] => "mcp".into(),
        ["sandbox", ":name", "ports", ..] => "ports".into(),
        ["sandbox", ..] => "sandbox".into(),
        [first, ..] => first.to_string(),
        _ => "other".into(),
    }
}

/// Builds an `operationId` from the method and the raw route, e.g.
/// `get_sandbox_name_exec_exec_id` for `GET /sandbox/:name/exec/:exec_id`.
fn operation_id(method: &str, route: &str) -> String {
    let mut s = method.to_lowercase();
    for seg in route.trim_start_matches('/').split('/') {
        let seg = seg.trim_start_matches(':');
        s.push('_');
        s.push_str(&seg.replace(['-', '.'], "_"));
    }
    s
}

/// Description of an observed HTTP status, for the OpenAPI `responses` object.
fn status_text(s: u16) -> &'static str {
    match s {
        101 => "Switching Protocols (raw stream after the upgrade)",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "Observed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keeps_every_any_of_alternative() {
        let object =
            json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]});
        let array = json!({"type": "array", "items": {"type": "string"}});
        let boolean = json!({"type": "boolean"});
        let mixed = merge(&object, &array);
        assert_eq!(mixed["anyOf"].as_array().map(Vec::len), Some(2));
        // Order of the samples must not matter, and duplicates are dropped.
        for merged in [merge(&mixed, &boolean), merge(&boolean, &mixed)] {
            let alternatives = merged["anyOf"].as_array().unwrap();
            assert_eq!(alternatives.len(), 3);
            assert!(alternatives.contains(&boolean));
        }
        assert_eq!(merge(&mixed, &array), mixed);
    }

    #[test]
    fn soften_required_only_renames_the_keyword() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "required": {"type": "boolean"},
                "nested": {"type": "object", "properties": {"x": {"type": "string"}}, "required": ["x"]},
            },
            "required": ["required", "nested"],
        });
        soften_required(&mut schema);
        assert!(schema.get("required").is_none());
        assert_eq!(schema["x-cli-always-sends"], json!(["required", "nested"]));
        assert_eq!(schema["properties"]["required"], json!({"type": "boolean"}));
        assert_eq!(
            schema["properties"]["nested"]["x-cli-always-sends"],
            json!(["x"])
        );
    }

    #[test]
    fn event_components_are_valid_keys() {
        assert_eq!(
            event_component("sandbox.state", "running"),
            "Event_sandbox_state_running"
        );
        assert_eq!(event_component("sync", ""), "Event_sync_none");
        assert_eq!(event_component("a/b", "c:d"), "Event_a_b_c_d");
    }

    #[test]
    fn daemon_log_patterns() {
        let dir = std::env::temp_dir().join(format!(
            "docker-sandbox-audit-spec-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("daemon.log");
        let lines = [
            json!({"msg": "http request", "method": "GET", "route": "/before", "status": 200}),
            json!({"msg": "http request", "method": "GET", "route": "", "status": 404}),
            json!({"msg": "http request", "method": "GET", "route": "/sandbox/:name", "status": 404}),
            json!({"msg": "http request", "method": "PUT", "route": "/sandbox/:name", "status": 405}),
        ];
        let text: Vec<String> = lines.iter().map(Value::to_string).collect();
        std::fs::write(&log, text.join("\n")).unwrap();
        let offset = u64::try_from(text[0].len() + 1).unwrap();
        let routes = router_log_routes(&log, offset).unwrap();
        let json_file = dir.join("router-log.json");
        let as_json: BTreeMap<&String, String> = routes
            .iter()
            .map(|(route, methods)| {
                (
                    route,
                    methods.iter().cloned().collect::<Vec<_>>().join(", "),
                )
            })
            .collect();
        std::fs::write(&json_file, json!({ "routes": as_json }).to_string()).unwrap();
        let patterns = load_patterns(None, Some(&json_file)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        // Lines before the offset are ignored; unmatched requests do not invent
        // a pattern; a 404 on a matched route keeps its method, a 405 does not.
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].raw, "/sandbox/:name");
        assert_eq!(patterns[0].methods, BTreeSet::from(["GET".to_owned()]));
    }
}
