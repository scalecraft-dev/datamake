//! `datamk mcp` (issue #32): the served interface as an MCP server over
//! stdio. Nothing here is a new door — every tool and resource is a skin
//! over the exact functions the REST routes call (`build_context_document`,
//! `execute_export_read`), on the same `AppState` `serve` would build, so
//! the two transports can never disagree about what is mounted, what a
//! query accepts, or which snapshot a supported route serves.
//!
//! The REST surface has three route shapes, so the MCP surface has three
//! tools, whatever the export count:
//!
//! - `list_exports`            <- `GET /context` (the export list, slim)
//! - `describe_export(route)`  <- `GET /context/{route}?include=docs`
//! - `query_export(route, …)`  <- `GET /{route}?…`
//!
//! The per-export schema lives where it already lives — in the context
//! document — rather than duplicated into `tools/list` as one tool per
//! export, which would make the agent's context cost scale with the cell.
//!
//! Transport: JSON-RPC 2.0, newline-delimited, hand-rolled. The subset a
//! closed grammar needs (`initialize`, `ping`, `tools/list`, `tools/call`,
//! `resources/list`, `resources/read`) is stateless request/response; an
//! SDK would buy notification and sampling plumbing this surface will never
//! use. No sessions, no subscriptions, no server-initiated messages.
//!
//! Auth: a stdio server runs as the user with whatever the profile grants —
//! exactly `datamk context -p <profile>`'s trust — so `authorize()` is not
//! consulted. `--no-data` is the withholding lever. The visibility filter
//! still applies: a `private` export appears nowhere (ADR 0012 §4).
//!
//! stdout belongs to the protocol. Tracing already goes to stderr
//! (`logging.rs`); `print_banner` never runs here.

use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::{
    build_context_document, execute_export_read, mount_project, mount_single,
    unknown_route_message, AppState, MountedCells, ReadOutcome, DEFAULT_LIMIT, MAX_LIMIT,
    MAX_OFFSET,
};

/// The protocol revisions this server speaks. It echoes whichever the
/// client asked for when that is one of these, else its own newest — the
/// negotiation the spec prescribes. Every method here is identical across
/// them; the list is a declaration, not a feature matrix.
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Serve one cell over stdio until stdin closes.
pub async fn run(file: &Path, profile: &str, poll_interval: u64, no_data: bool) -> Result<()> {
    let (state, _row) = mount_single(file, profile, poll_interval, no_data)?;
    let server = McpServer::single(state);
    serve_stdio(Arc::new(server)).await
}

/// Serve every cell a project file lists over one stdio transport (ADR
/// 0014's mounting rules, unchanged): routes are qualified by mount.
pub async fn run_project(
    project: &crate::project::Project,
    poll_interval: u64,
    no_data: bool,
) -> Result<()> {
    let (mounted, _rows) = mount_project(project, poll_interval, no_data)?;
    let server = McpServer::project(mounted);
    serve_stdio(Arc::new(server)).await
}

/// The stdio loop: one JSON-RPC message per line in, one per line out.
/// stdin is read on a plain thread (the runtime's `io-std` feature is not
/// enabled, and there is no reason to add it for one blocking reader);
/// each request is dispatched on the runtime so a slow query never blocks
/// a `ping`. Responses are written whole, under a lock, so concurrent
/// completions can't interleave partial lines. EOF on stdin is the
/// client's "goodbye": in-flight requests finish, then the process exits.
async fn serve_stdio(server: Arc<McpServer>) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    std::thread::Builder::new()
        .name("mcp-stdin".to_string())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut line = String::new();
            loop {
                line.clear();
                match std::io::BufRead::read_line(&mut stdin.lock(), &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx
                            .blocking_send(line.trim_end_matches(['\n', '\r']).to_string())
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        })?;

    let out = Arc::new(Mutex::new(std::io::stdout()));
    let mut in_flight = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            line = rx.recv() => {
                let Some(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let server = server.clone();
                let out = out.clone();
                in_flight.spawn(async move {
                    if let Some(reply) = server.handle_line(&line).await {
                        write_line(&out, &reply);
                    }
                });
            }
            _ = super::shutdown_signal() => break,
        }
    }
    while in_flight.join_next().await.is_some() {}
    Ok(())
}

fn write_line(out: &Mutex<std::io::Stdout>, msg: &Value) {
    let mut out = out.lock().expect("stdout mutex poisoned");
    // A write failure means the client went away; there is nobody left to
    // tell, and the read loop will see EOF and exit.
    let _ = writeln!(out, "{msg}");
    let _ = out.flush();
}

/// One mounted cell as MCP sees it: the mount segment that qualifies its
/// routes and resource URIs (the cell name at the root mount), and the
/// serving state `serve` would use.
struct Mount {
    name: String,
    state: Arc<AppState>,
}

pub(super) struct McpServer {
    mounts: Vec<Mount>,
    /// Project mode: routes are `<mount>/<route>` on every tool, and
    /// `list_exports` says which cell each came from.
    project: bool,
}

impl McpServer {
    pub(super) fn single(state: Arc<AppState>) -> Self {
        let name = state.cell_name.clone();
        McpServer {
            mounts: vec![Mount { name, state }],
            project: false,
        }
    }

    pub(super) fn project(mounted: MountedCells) -> Self {
        McpServer {
            mounts: mounted
                .into_iter()
                .map(|(name, state)| Mount { name, state })
                .collect(),
            project: true,
        }
    }

    /// Dispatch one raw line. `None` means nothing goes back (a
    /// notification, or an unparseable line that carried no id to answer).
    pub(super) async fn handle_line(&self, line: &str) -> Option<Value> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(rpc_error(Value::Null, -32700, format!("parse error: {e}")));
            }
        };
        self.handle(msg).await
    }

    /// Dispatch one parsed JSON-RPC message.
    pub(super) async fn handle(&self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        // Notifications carry no id and get no reply, whatever they say.
        let id = id?;
        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": self.tools() })),
            "tools/call" => self.call_tool(&params).await,
            "resources/list" => Ok(json!({ "resources": self.resources() })),
            "resources/read" => self.read_resource(&params),
            "" => Err((-32600, "invalid request: no method".to_string())),
            other => Err((-32601, format!("method not found: {other}"))),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => rpc_error(id, code, message),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let asked = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("");
        let version = if PROTOCOL_VERSIONS.contains(&asked) {
            asked
        } else {
            PROTOCOL_VERSIONS[0]
        };
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": {}, "resources": {} },
            "serverInfo": {
                "name": "datamk",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": self.instructions(),
        })
    }

    fn instructions(&self) -> String {
        let cells: Vec<&str> = self.mounts.iter().map(|m| m.name.as_str()).collect();
        let scope = if self.project {
            format!(
                "This server mounts {} cells ({}); every `route` is `<mount>/<name>@<major>`.",
                cells.len(),
                cells.join(", ")
            )
        } else {
            format!("This server mounts one cell: {}.", cells[0])
        };
        format!(
            "datamk serves verified data products (cells). {scope} Start with \
             `list_exports` to see what exists and what each row means; call \
             `describe_export` for one export's full schema, filters, and docs \
             before querying it; then `query_export` for rows. Queries are a closed \
             grammar: exact-equality filters on the export's grain columns only, plus \
             `limit` (default {DEFAULT_LIMIT}, max {MAX_LIMIT}) and `offset` (max \
             {MAX_OFFSET}). There is no SQL, no ranges, no aggregation, and no way to \
             read a column that is not declared. A result with `truncated: true` is one \
             full page, not the whole export — follow `next`."
        )
    }

    // ---- tools -----------------------------------------------------------

    fn tools(&self) -> Vec<Value> {
        let route_desc = if self.project {
            "The export's route, qualified by mount: `<mount>/<name>@<major>` \
             (from `list_exports`)."
        } else {
            "The export's route key, `<name>@<major>` (from `list_exports`)."
        };
        vec![
            json!({
                "name": "list_exports",
                "description": "List every export this server mounts: route, what one row \
                    means, grain columns (the only filterable columns), contract level, and \
                    the author's declared freshness (a claim, not a measurement). Call this \
                    first.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            }),
            json!({
                "name": "describe_export",
                "description": "One export's full contract: column-by-column schema and \
                    meanings, the exact filters `query_export` accepts, limits, a sample \
                    request, definitions, and any long-form docs inlined. The same \
                    document GET /context/{route}?include=docs serves.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "route": { "type": "string", "description": route_desc }
                    },
                    "required": ["route"],
                    "additionalProperties": false
                }
            }),
            json!({
                "name": "query_export",
                "description": format!(
                    "Read rows from one export. `filters` is an object of grain column -> \
                     value, exact equality only — no ranges, no operators, no non-grain \
                     columns; an unknown filter is an error, never ignored. Rows come back \
                     ordered by grain. `limit` defaults to {DEFAULT_LIMIT} (max {MAX_LIMIT}); \
                     `offset` max {MAX_OFFSET}. Supported-contract exports serve their \
                     released snapshot; experimental ones serve latest."
                ),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "route": { "type": "string", "description": route_desc },
                        "filters": {
                            "type": "object",
                            "description": "grain column -> value (exact equality). \
                                `describe_export` lists the grain.",
                            "additionalProperties": {
                                "type": ["string", "number", "boolean"]
                            }
                        },
                        "limit": {
                            "type": "integer", "minimum": 0, "maximum": MAX_LIMIT,
                            "default": DEFAULT_LIMIT
                        },
                        "offset": {
                            "type": "integer", "minimum": 0, "maximum": MAX_OFFSET,
                            "default": 0
                        }
                    },
                    "required": ["route"],
                    "additionalProperties": false
                }
            }),
        ]
    }

    async fn call_tool(&self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let Some(args) = args.as_object().cloned() else {
            return Err((-32602, "`arguments` must be an object".to_string()));
        };
        match name {
            "list_exports" => {
                reject_unknown_args(&args, &[])?;
                Ok(tool_ok(self.list_exports()))
            }
            "describe_export" => {
                reject_unknown_args(&args, &["route"])?;
                let route = required_str(&args, "route")?;
                Ok(match self.describe_export(route) {
                    Ok(doc) => tool_ok(doc),
                    Err(msg) => tool_err(msg),
                })
            }
            "query_export" => {
                reject_unknown_args(&args, &["route", "filters", "limit", "offset"])?;
                let route = required_str(&args, "route")?;
                Ok(match self.query_export(route, &args).await {
                    Ok(v) => tool_ok(v),
                    Err(msg) => tool_err(msg),
                })
            }
            other => Err((
                -32602,
                format!(
                    "unknown tool '{other}' — this server has: list_exports, describe_export, \
                     query_export"
                ),
            )),
        }
    }

    fn list_exports(&self) -> Value {
        let mut cells = Vec::new();
        let mut exports = Vec::new();
        for m in &self.mounts {
            let doc = build_context_document(&m.state);
            cells.push(json!({
                "mount": m.name,
                "cell": doc.cell,
                "status": doc.status,
                "grain_verified": doc.grain_verified,
                "description": doc.description,
                "resource": self.context_uri(m, None),
            }));
            for e in &doc.exports {
                exports.push(json!({
                    "route": self.qualify(m, &e.route),
                    "cell": doc.cell,
                    "description": e.description,
                    "grain": e.grain,
                    "contract": e.contract,
                    "freshness": e.freshness,
                    // `query` is null exactly when `GET /{route}` does not
                    // exist (a bound export, issue #6) — the machine-checkable
                    // "can I call query_export on this" signal.
                    "queryable": e.query.is_some() && m.state.data_mounted,
                    "resource": self.context_uri(m, Some(&e.route)),
                }));
            }
        }
        json!({ "cells": cells, "exports": exports })
    }

    fn describe_export(&self, qualified: &str) -> Result<Value, String> {
        let (m, route) = self.resolve(qualified)?;
        if !m.state.routes.contains_key(route) {
            return Err(unknown_route_message(&m.state, route));
        }
        let doc = narrowed_with_docs(&m.state, route);
        let mut v = serde_json::to_value(doc).map_err(|e| e.to_string())?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("resource".into(), json!(self.context_uri(m, Some(route))));
        }
        Ok(v)
    }

    async fn query_export(
        &self,
        qualified: &str,
        args: &serde_json::Map<String, Value>,
    ) -> Result<Value, String> {
        let (m, route) = self.resolve(qualified)?;
        // --no-data (ADR 0012 §4): the data door is not mounted. Same
        // sentence as the REST 404 and the document's notes.
        if !m.state.data_mounted {
            return Err(crate::context::NOTE_NO_DATA.to_string());
        }
        let params = flatten_query_args(args)?;
        match execute_export_read(&m.state, route, &params).await {
            ReadOutcome::Rows {
                rows,
                limit,
                offset,
            } => {
                let rows: Vec<Value> = rows
                    .iter()
                    .map(|r| serde_json::from_str(r).unwrap_or_else(|_| Value::String(r.clone())))
                    .collect();
                let row_count = rows.len();
                // "A full page — there may be more", not a total-count
                // claim the server cannot make.
                let truncated = limit > 0 && row_count == limit;
                Ok(json!({
                    "route": qualified,
                    "rows": rows,
                    "row_count": row_count,
                    "limit": limit,
                    "offset": offset,
                    "truncated": truncated,
                    "next": truncated.then(|| json!({ "offset": offset + limit })),
                    "resource": self.context_uri(m, Some(route)),
                }))
            }
            ReadOutcome::NotFound(_) => Err(unknown_route_message(&m.state, route)),
            ReadOutcome::Bound(msg) | ReadOutcome::BadParams(msg) | ReadOutcome::Internal(msg) => {
                Err(msg)
            }
        }
    }

    // ---- resources -------------------------------------------------------

    fn resources(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for m in &self.mounts {
            let doc = build_context_document(&m.state);
            out.push(json!({
                "uri": self.context_uri(m, None),
                "name": format!("{} context", doc.cell),
                "description": "The cell's context document: every export, its grain, \
                    schema, meaning, query grammar, and provenance (GET /context).",
                "mimeType": "application/json",
            }));
            for e in &doc.exports {
                out.push(json!({
                    "uri": self.context_uri(m, Some(&e.route)),
                    "name": format!("{} {}", doc.cell, e.route),
                    "description": e.description.clone().unwrap_or_else(|| {
                        format!("The context document narrowed to {}", e.route)
                    }),
                    "mimeType": "application/json",
                }));
            }
            for p in &m.state.docs_pages {
                out.push(json!({
                    "uri": format!("datamk://{}/docs/{}", m.name, p.target),
                    "name": format!("{} docs: {}", doc.cell, p.target),
                    "description": format!("Long-form documentation for `{}`", p.target),
                    "mimeType": p.media_type,
                }));
            }
        }
        out
    }

    fn read_resource(&self, params: &Value) -> Result<Value, (i64, String)> {
        let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
        let not_found = |why: String| (-32002, format!("resource not found: {uri} — {why}"));
        let rest = uri
            .strip_prefix("datamk://")
            .ok_or_else(|| not_found("URIs are datamk://<mount>/…".to_string()))?;
        let (mount, path) = rest
            .split_once('/')
            .ok_or_else(|| not_found("expected datamk://<mount>/context".to_string()))?;
        let m = self
            .mounts
            .iter()
            .find(|m| m.name == mount)
            .ok_or_else(|| {
                let known: Vec<&str> = self.mounts.iter().map(|m| m.name.as_str()).collect();
                not_found(format!("mounted cells: {}", known.join(", ")))
            })?;
        let text = if path == "context" {
            serde_json::to_string(&build_context_document(&m.state))
        } else if let Some(route) = path.strip_prefix("context/") {
            if !m.state.routes.contains_key(route) {
                return Err(not_found(unknown_route_message(&m.state, route)));
            }
            serde_json::to_string(&narrowed_with_docs(&m.state, route))
        } else if let Some(target) = path.strip_prefix("docs/") {
            let page = m
                .state
                .docs_pages
                .iter()
                .find(|p| p.target == target)
                .ok_or_else(|| {
                    let known: Vec<&str> = m
                        .state
                        .docs_pages
                        .iter()
                        .map(|p| p.target.as_str())
                        .collect();
                    not_found(format!(
                        "docs targets: {}",
                        if known.is_empty() {
                            "none".to_string()
                        } else {
                            known.join(", ")
                        }
                    ))
                })?;
            return Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": page.media_type,
                    "text": page.content.as_ref(),
                }]
            }));
        } else {
            return Err(not_found(
                "paths are `context`, `context/<route>`, `docs/<target>`".to_string(),
            ));
        }
        .map_err(|e| (-32603, e.to_string()))?;
        Ok(json!({
            "contents": [{ "uri": uri, "mimeType": "application/json", "text": text }]
        }))
    }

    // ---- naming ----------------------------------------------------------

    /// `<mount>/<route>` in project mode, the bare route otherwise.
    fn qualify(&self, m: &Mount, route: &str) -> String {
        if self.project {
            format!("{}/{route}", m.name)
        } else {
            route.to_string()
        }
    }

    /// The inverse of `qualify`: which mount, which route. Single-cell mode
    /// takes the bare route (a `mount/` prefix naming the one cell is also
    /// accepted, so a client can't be wrong by being explicit).
    fn resolve<'a>(&'a self, qualified: &'a str) -> Result<(&'a Mount, &'a str), String> {
        if let Some((mount, route)) = qualified.split_once('/') {
            if let Some(m) = self.mounts.iter().find(|m| m.name == mount) {
                return Ok((m, route));
            }
            let known: Vec<&str> = self.mounts.iter().map(|m| m.name.as_str()).collect();
            return Err(format!(
                "no mounted cell '{mount}' — mounted cells: {}",
                known.join(", ")
            ));
        }
        if self.project {
            let known: Vec<&str> = self.mounts.iter().map(|m| m.name.as_str()).collect();
            return Err(format!(
                "route '{qualified}' must be qualified by mount (`<mount>/<name>@<major>`) — \
                 mounted cells: {}. See list_exports.",
                known.join(", ")
            ));
        }
        Ok((&self.mounts[0], qualified))
    }

    fn context_uri(&self, m: &Mount, route: Option<&str>) -> String {
        match route {
            Some(r) => format!("datamk://{}/context/{r}", m.name),
            None => format!("datamk://{}/context", m.name),
        }
    }
}

/// The document narrowed to one route with its docs inlined — what
/// `GET /context/{route}?include=docs` serves, minus ETag mechanics.
fn narrowed_with_docs(s: &AppState, route: &str) -> crate::context::ContextDocument {
    let mut doc = build_context_document(s);
    doc.narrow_to(route);
    let targets: std::collections::HashSet<&str> =
        doc.docs.iter().map(|d| d.target.as_str()).collect();
    let pages: Vec<&crate::config::docs::DocsPage> = s
        .docs_pages
        .iter()
        .filter(|p| targets.contains(p.target.as_str()))
        .collect();
    doc.inline_docs(pages);
    doc
}

/// `query_export`'s arguments -> the flat `?k=v` map the REST door parses,
/// so `validate_params` is the one arbiter of the grammar. Values must be
/// scalars: a nested object or array has no query-string spelling and no
/// meaning under exact equality.
fn flatten_query_args(
    args: &serde_json::Map<String, Value>,
) -> Result<HashMap<String, String>, String> {
    let mut params = HashMap::new();
    if let Some(filters) = args.get("filters") {
        let Some(filters) = filters.as_object() else {
            return Err("`filters` must be an object of grain column -> value".to_string());
        };
        for (k, v) in filters {
            params.insert(k.clone(), scalar_string(k, v)?);
        }
    }
    for k in ["limit", "offset"] {
        if let Some(v) = args.get(k) {
            if v.is_null() {
                continue;
            }
            // A filter column literally named `limit`/`offset` would be
            // shadowed here exactly as it is on the query string.
            params.insert(k.to_string(), scalar_string(k, v)?);
        }
    }
    Ok(params)
}

fn scalar_string(k: &str, v: &Value) -> Result<String, String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Null => Err(format!(
            "filter `{k}` is null — omit it to leave the column unfiltered"
        )),
        _ => Err(format!(
            "filter `{k}` must be a scalar (string, number, or boolean) — exact equality only"
        )),
    }
}

/// The schema-level `additionalProperties: false`, enforced server-side too:
/// an argument the tool does not declare is an error, never dropped — the
/// same rule `validate_params` applies on the query string.
fn reject_unknown_args(
    args: &serde_json::Map<String, Value>,
    allowed: &[&str],
) -> Result<(), (i64, String)> {
    for k in args.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err((
                -32602,
                format!(
                    "unknown argument '{k}' — this tool accepts: {}",
                    if allowed.is_empty() {
                        "no arguments".to_string()
                    } else {
                        allowed.join(", ")
                    }
                ),
            ));
        }
    }
    Ok(())
}

fn required_str<'a>(
    args: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<&'a str, (i64, String)> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            (
                -32602,
                format!("`{key}` is required and must be a non-empty string"),
            )
        })
}

/// A successful tool result: the JSON both as text (every client renders
/// this) and as `structuredContent` (clients that understand it get typed
/// access without re-parsing).
fn tool_ok(v: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": v.to_string() }],
        "structuredContent": v,
        "isError": false
    })
}

/// A tool-level failure (the spec's `isError`, not a protocol error): the
/// model sees the message and can self-correct — which is the point of
/// carrying REST's own 400/404 sentences verbatim.
fn tool_err(msg: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": msg }],
        "isError": true
    })
}

fn rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

/// Driven through `McpServer::handle` directly — no process, no pipes —
/// on the same built scaffolds the REST smoke tests use, so a tool result
/// can be asserted against the exact document the REST door would serve.
#[cfg(test)]
mod tests {
    use super::super::smoke::{built_cell, project_mode};
    use super::super::{build_state, DEFAULT_MAX_CONCURRENCY};
    use super::*;

    fn single(no_data: bool) -> McpServer {
        let scaffold = built_cell();
        let cell_yaml = scaffold.dir.join("cell.yaml");
        let cell = crate::engine::open(&cell_yaml, "local", true).expect("open built cell");
        let (state, _store) =
            build_state(cell, no_data, &cell_yaml, "local", "").expect("build state");
        let _ = DEFAULT_MAX_CONCURRENCY;
        McpServer::single(state)
    }

    fn project() -> McpServer {
        let a = project_mode::mounted(built_cell(), "smoke", false, |_| {});
        let b = project_mode::mounted(project_mode::built_orders_cell(), "orders", false, |_| {});
        McpServer::project(vec![a, b])
    }

    async fn call(server: &McpServer, id: u64, method: &str, params: Value) -> Value {
        server
            .handle(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await
            .expect("a request with an id gets a reply")
    }

    async fn tool(server: &McpServer, name: &str, args: Value) -> Value {
        let v = call(
            server,
            1,
            "tools/call",
            json!({ "name": name, "arguments": args }),
        )
        .await;
        assert!(
            v.get("error").is_none(),
            "tool call was a protocol error: {v}"
        );
        v["result"].clone()
    }

    fn text(result: &Value) -> &str {
        result["content"][0]["text"].as_str().unwrap()
    }

    #[tokio::test]
    async fn initialize_echoes_a_known_version_and_names_the_cell() {
        let s = single(false);
        let v = call(
            &s,
            1,
            "initialize",
            json!({ "protocolVersion": "2025-03-26", "capabilities": {} }),
        )
        .await;
        assert_eq!(v["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(v["result"]["serverInfo"]["name"], "datamk");
        let instructions = v["result"]["instructions"].as_str().unwrap();
        assert!(instructions.contains("one cell: smoke"), "{instructions}");
        assert!(instructions.contains("no SQL"), "{instructions}");

        // An unknown version gets our newest, never an error.
        let v = call(
            &s,
            2,
            "initialize",
            json!({ "protocolVersion": "1999-01-01" }),
        )
        .await;
        assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    /// The whole point of the fixed set: the tool count does not move with
    /// the export count.
    #[tokio::test]
    async fn tools_list_is_exactly_three_in_both_modes() {
        for s in [single(false), project()] {
            let v = call(&s, 1, "tools/list", Value::Null).await;
            let names: Vec<&str> = v["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap())
                .collect();
            assert_eq!(names, ["list_exports", "describe_export", "query_export"]);
            for t in v["result"]["tools"].as_array().unwrap() {
                assert_eq!(
                    t["inputSchema"]["additionalProperties"], false,
                    "closed grammar, declared: {t}"
                );
            }
        }
    }

    #[tokio::test]
    async fn protocol_errors_have_the_spec_codes() {
        let s = single(false);
        assert!(
            s.handle(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
                .await
                .is_none(),
            "a notification gets no reply"
        );
        let v = s.handle_line("{not json").await.unwrap();
        assert_eq!(v["error"]["code"], -32700);
        assert_eq!(v["id"], Value::Null);

        let v = call(&s, 1, "nope", Value::Null).await;
        assert_eq!(v["error"]["code"], -32601);

        let v = call(
            &s,
            1,
            "tools/call",
            json!({ "name": "sql", "arguments": {} }),
        )
        .await;
        assert_eq!(v["error"]["code"], -32602);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("list_exports"));

        // An argument the schema does not declare is an error, not dropped.
        let v = call(
            &s,
            1,
            "tools/call",
            json!({ "name": "query_export", "arguments": { "route": "orders_daily@2", "where": "1=1" } }),
        )
        .await;
        assert_eq!(v["error"]["code"], -32602, "{v}");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown argument 'where'"));

        let v = call(
            &s,
            1,
            "resources/read",
            json!({ "uri": "datamk://smoke/sql" }),
        )
        .await;
        assert_eq!(v["error"]["code"], -32002, "{v}");
    }

    #[tokio::test]
    async fn list_exports_is_the_context_documents_export_list() {
        let s = single(false);
        let r = tool(&s, "list_exports", json!({})).await;
        assert_eq!(r["isError"], false);
        let sc = &r["structuredContent"];
        assert_eq!(sc["cells"][0]["cell"], "smoke");
        assert_eq!(sc["cells"][0]["resource"], "datamk://smoke/context");
        let e = &sc["exports"][0];
        assert_eq!(e["route"], "orders_daily@2");
        assert_eq!(e["grain"], json!(["order_date", "region"]));
        assert_eq!(e["queryable"], true);
        assert_eq!(e["resource"], "datamk://smoke/context/orders_daily@2");
        // text and structuredContent carry the same JSON.
        let from_text: Value = serde_json::from_str(text(&r)).unwrap();
        assert_eq!(&from_text, sc);
    }

    #[tokio::test]
    async fn describe_export_is_the_narrowed_document_with_docs_inlined() {
        let s = single(false);
        let r = tool(&s, "describe_export", json!({ "route": "orders_daily@2" })).await;
        assert_eq!(r["isError"], false, "{r}");
        let doc = &r["structuredContent"];
        assert_eq!(doc["datamk_context"], 4);
        assert_eq!(doc["exports"].as_array().unwrap().len(), 1);
        assert_eq!(doc["exports"][0]["query"]["limit_max"], MAX_LIMIT);
        assert_eq!(doc["resource"], "datamk://smoke/context/orders_daily@2");

        let r = tool(&s, "describe_export", json!({ "route": "nope@1" })).await;
        assert_eq!(r["isError"], true);
        assert!(
            text(&r).contains("no export 'nope@1' — discoverable exports: orders_daily@2"),
            "{}",
            text(&r)
        );
    }

    #[tokio::test]
    async fn query_export_serves_a_page_and_discloses_it() {
        let s = single(false);
        let r = tool(
            &s,
            "query_export",
            json!({ "route": "orders_daily@2", "limit": 1 }),
        )
        .await;
        assert_eq!(r["isError"], false, "{r}");
        let sc = &r["structuredContent"];
        assert_eq!(sc["limit"], 1);
        assert_eq!(sc["offset"], 0);
        assert_eq!(sc["row_count"], 1);
        assert_eq!(sc["truncated"], true, "a full page may have more");
        assert_eq!(sc["next"]["offset"], 1);
        assert!(
            sc["rows"][0].is_object(),
            "rows are objects, not strings: {sc}"
        );
        assert_eq!(sc["resource"], "datamk://smoke/context/orders_daily@2");

        // Numbers and strings both spell a filter value; the grammar is
        // still the REST one.
        let r = tool(
            &s,
            "query_export",
            json!({ "route": "orders_daily@2", "filters": { "region": "nowhere" } }),
        )
        .await;
        assert_eq!(r["structuredContent"]["row_count"], 0);
        assert_eq!(r["structuredContent"]["truncated"], false);
        assert_eq!(r["structuredContent"]["next"], Value::Null);
    }

    /// The issue's one hard requirement: an unknown filter fails the way
    /// `serve` fails it — the same sentence — never silently ignored.
    #[tokio::test]
    async fn query_export_rejects_non_grain_filters_with_serves_own_sentence() {
        let s = single(false);
        let r = tool(
            &s,
            "query_export",
            json!({ "route": "orders_daily@2", "filters": { "revenue": 999 } }),
        )
        .await;
        assert_eq!(r["isError"], true);
        let msg = text(&r);
        assert!(
            msg.starts_with("unknown query parameter 'revenue'"),
            "{msg}"
        );
        assert!(msg.contains("grain filters (order_date, region)"), "{msg}");

        let r = tool(
            &s,
            "query_export",
            json!({ "route": "orders_daily@2", "filters": { "region": ["a", "b"] } }),
        )
        .await;
        assert_eq!(r["isError"], true);
        assert!(text(&r).contains("must be a scalar"), "{}", text(&r));

        let r = tool(
            &s,
            "query_export",
            json!({ "route": "orders_daily@2", "offset": MAX_OFFSET + 1 }),
        )
        .await;
        assert_eq!(r["isError"], true);
        assert!(text(&r).contains("exceeds the maximum"), "{}", text(&r));
    }

    #[tokio::test]
    async fn no_data_describes_but_does_not_serve_rows() {
        let s = single(true);
        let r = tool(&s, "list_exports", json!({})).await;
        assert_eq!(r["structuredContent"]["exports"][0]["queryable"], false);
        let r = tool(&s, "describe_export", json!({ "route": "orders_daily@2" })).await;
        assert_eq!(r["isError"], false);
        let r = tool(&s, "query_export", json!({ "route": "orders_daily@2" })).await;
        assert_eq!(r["isError"], true);
        assert_eq!(text(&r), crate::context::NOTE_NO_DATA);
    }

    #[tokio::test]
    async fn resources_are_the_context_document_by_uri() {
        let s = single(false);
        let v = call(&s, 1, "resources/list", Value::Null).await;
        let uris: Vec<&str> = v["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["uri"].as_str().unwrap())
            .collect();
        assert!(uris.contains(&"datamk://smoke/context"), "{uris:?}");
        assert!(
            uris.contains(&"datamk://smoke/context/orders_daily@2"),
            "{uris:?}"
        );

        let v = call(
            &s,
            1,
            "resources/read",
            json!({ "uri": "datamk://smoke/context" }),
        )
        .await;
        let c = &v["result"]["contents"][0];
        assert_eq!(c["mimeType"], "application/json");
        let doc: Value = serde_json::from_str(c["text"].as_str().unwrap()).unwrap();
        let expected = serde_json::to_value(build_context_document(&s.mounts[0].state)).unwrap();
        assert_eq!(
            doc, expected,
            "the resource is the REST document, byte for byte"
        );

        let v = call(
            &s,
            1,
            "resources/read",
            json!({ "uri": "datamk://smoke/context/nope@1" }),
        )
        .await;
        assert_eq!(v["error"]["code"], -32002);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("orders_daily@2"));

        let v = call(
            &s,
            1,
            "resources/read",
            json!({ "uri": "datamk://other/context" }),
        )
        .await;
        assert_eq!(v["error"]["code"], -32002);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("mounted cells: smoke"));
    }

    /// Project mode (ADR 0014): routes are qualified by mount on every tool,
    /// the tool count still does not move, and a bare route is told how to
    /// qualify itself rather than guessed at.
    #[tokio::test]
    async fn project_mode_qualifies_routes_by_mount() {
        let s = project();
        let r = tool(&s, "list_exports", json!({})).await;
        let sc = &r["structuredContent"];
        let mounts: Vec<&str> = sc["cells"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["mount"].as_str().unwrap())
            .collect();
        assert_eq!(mounts, ["smoke", "orders"]);
        let routes: Vec<&str> = sc["exports"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["route"].as_str().unwrap())
            .collect();
        assert!(routes.contains(&"smoke/orders_daily@2"), "{routes:?}");
        assert!(routes.contains(&"orders/orders_daily@2"), "{routes:?}");

        let r = tool(
            &s,
            "query_export",
            json!({ "route": "orders/orders_daily@2", "limit": 1 }),
        )
        .await;
        assert_eq!(r["isError"], false, "{r}");
        assert_eq!(r["structuredContent"]["route"], "orders/orders_daily@2");
        assert_eq!(
            r["structuredContent"]["resource"],
            "datamk://orders/context/orders_daily@2"
        );

        let r = tool(&s, "query_export", json!({ "route": "orders_daily@2" })).await;
        assert_eq!(r["isError"], true);
        assert!(
            text(&r).contains("must be qualified by mount"),
            "{}",
            text(&r)
        );
        assert!(text(&r).contains("smoke, orders"), "{}", text(&r));

        let r = tool(
            &s,
            "describe_export",
            json!({ "route": "nope/orders_daily@2" }),
        )
        .await;
        assert_eq!(r["isError"], true);
        assert!(text(&r).contains("no mounted cell 'nope'"), "{}", text(&r));
    }
}
