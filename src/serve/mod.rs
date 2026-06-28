mod openapi;

use anyhow::Result;
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::config::{Contract, Export, Visibility};
use crate::engine;

const MAX_LIMIT: usize = 1000;
const DEFAULT_LIMIT: usize = 100;

struct AppState {
    conn: Mutex<duckdb::Connection>,
    /// route key (`name@major`) -> export
    routes: HashMap<String, Export>,
    /// route key -> pinned snapshot id (from the publish manifest)
    published: BTreeMap<String, i64>,
    openapi: serde_json::Value,
    cell: String,
    /// Authorization policy (default-deny).
    shareable: bool,
    allowed_roles: Vec<String>,
    /// bearer token -> roles
    principals: HashMap<String, Vec<String>>,
}

/// Result of an authorization check: an error response, or pass.
fn authorize(s: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    if !s.shareable {
        return Err((StatusCode::FORBIDDEN, "cell is not shareable").into_response());
    }
    if s.allowed_roles.is_empty() {
        return Ok(()); // shareable + no roles = open
    }
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    let token = match token {
        Some(t) if !t.is_empty() => t,
        _ => return Err((StatusCode::UNAUTHORIZED, "missing bearer token").into_response()),
    };
    let roles = match s.principals.get(token) {
        Some(r) => r,
        None => return Err((StatusCode::UNAUTHORIZED, "unknown token").into_response()),
    };
    if roles.iter().any(|r| s.allowed_roles.contains(r)) {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "insufficient role").into_response())
    }
}

fn load_principals(path: Option<&str>) -> HashMap<String, Vec<String>> {
    path.and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Serve the declared interface as REST + OpenAPI (the Server workload).
pub async fn run(file: &Path, profile: &str, port: u16) -> Result<()> {
    let cell = engine::open(file, profile, /* read_only */ true)?;
    let published = load_published(&cell.dir);

    let mut routes = HashMap::new();
    for export in &cell.def.interface {
        if export.visibility == Visibility::Discoverable {
            routes.insert(export.route()?, export.clone());
        }
    }

    let principals = load_principals(cell.principals.as_deref());
    if !cell.def.access.roles.is_empty() && principals.is_empty() {
        tracing::warn!("access.roles set but no principals loaded; all authorized requests will be denied");
    }

    let state = Arc::new(AppState {
        openapi: openapi::generate(&cell.def),
        cell: cell.def.cell.clone(),
        conn: Mutex::new(cell.conn),
        routes,
        published,
        shareable: cell.def.access.shareable,
        allowed_roles: cell.def.access.roles.clone(),
        principals,
    });

    let app = Router::new()
        .route("/", get(health))
        .route("/interface", get(interface))
        .route("/openapi.json", get(openapi_doc))
        .route("/:route", get(serve_export))
        .with_state(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(%addr, "serving cell");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn load_published(dir: &Path) -> BTreeMap<String, i64> {
    let path = dir.join(".cell").join("published.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<crate::publish::Published>(&raw).ok())
        .map(|p| p.routes)
        .unwrap_or_default()
}

async fn health(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "cell": s.cell, "status": "ok" }))
}

async fn interface(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = authorize(&s, &headers) {
        return resp;
    }
    let mut exports: Vec<_> = s.routes.keys().cloned().collect();
    exports.sort();
    Json(serde_json::json!({ "cell": s.cell, "exports": exports })).into_response()
}

async fn openapi_doc(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = authorize(&s, &headers) {
        return resp;
    }
    Json(s.openapi.clone()).into_response()
}

async fn serve_export(
    State(s): State<Arc<AppState>>,
    AxumPath(route): AxumPath<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = authorize(&s, &headers) {
        return resp;
    }
    let export = match s.routes.get(&route) {
        Some(e) => e.clone(),
        None => return (StatusCode::NOT_FOUND, format!("no export '{route}'")).into_response(),
    };

    // Supported contracts serve a pinned snapshot; experimental tracks latest.
    let snapshot = if export.contract == Contract::Supported {
        s.published.get(&route).copied()
    } else {
        None
    };

    let sql = build_query(&export, &params, snapshot);

    let s2 = s.clone();
    let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<String>> {
        let conn = s2.conn.lock().expect("connection mutex poisoned");
        run_json_query(&conn, &sql)
    })
    .await;

    match rows {
        Ok(Ok(rows)) => (
            [(header::CONTENT_TYPE, "application/json")],
            format!("[{}]", rows.join(",")),
        )
            .into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Build the read query for an export. Only declared columns and grain columns
/// reach the SQL (both come from the cell definition, not user input); grain
/// filter *values* are escaped. Pagination is capped.
fn build_query(export: &Export, params: &HashMap<String, String>, snapshot: Option<i64>) -> String {
    let cols = if export.schema.is_empty() {
        "*".to_string()
    } else {
        export.schema.keys().cloned().collect::<Vec<_>>().join(", ")
    };
    let source = export.source_object();
    let at = match snapshot {
        Some(id) => format!(" AT (VERSION => {id})"),
        None => String::new(),
    };

    let mut wheres = Vec::new();
    for g in &export.grain {
        if let Some(v) = params.get(g) {
            wheres.push(format!("{g} = '{}'", v.replace('\'', "''")));
        }
    }
    let where_clause = if wheres.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", wheres.join(" AND "))
    };

    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_LIMIT)
        .min(MAX_LIMIT);
    let offset = params
        .get("offset")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);

    format!(
        "SELECT to_json(t) AS j FROM \
         (SELECT {cols} FROM {source}{at}{where_clause} LIMIT {limit} OFFSET {offset}) t"
    )
}

fn run_json_query(conn: &duckdb::Connection, sql: &str) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}
