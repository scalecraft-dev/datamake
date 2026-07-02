mod openapi;

use anyhow::{Context, Result};
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
    /// route key -> pinned snapshot id (from the release manifest)
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
// The `Err` variant is an axum `Response` by design — callers early-return it
// directly. The size is acceptable on the unauthorized path; boxing every call
// site would only add noise.
#[allow(clippy::result_large_err)]
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

/// Load the bearer-token -> roles map from the path the profile's `principals:`
/// names. Fails loud when a path is set but unreadable or malformed: a swallowed
/// error would silently start an all-deny server (or, worse, look healthy via
/// `/health` while denying every request). No path = no principals (an open
/// endpoint, gated upstream by `shareable`/`allow_anonymous`).
fn load_principals(path: Option<&str>) -> Result<HashMap<String, Vec<String>>> {
    let Some(p) = path else {
        return Ok(HashMap::new());
    };
    let raw = std::fs::read_to_string(p)
        .with_context(|| format!("reading principals file {p} (referenced by `principals:`)"))?;
    parse_principals(&raw).with_context(|| format!("parsing principals file {p}"))
}

/// The JSON-parse core of `load_principals`, split out so the Kubernetes
/// deploy pre-flight (ADR 0002 §6) can validate the *same* bytes — straight off
/// a live Secret, no temp file — with the exact same rules `serve` itself
/// applies. A deploy that passes this check can't yield a pod that starts up
/// and silently denies every request on malformed JSON (ADR 0002 §6).
pub(crate) fn parse_principals(raw: &str) -> Result<HashMap<String, Vec<String>>> {
    serde_json::from_str(raw)
        .with_context(|| "parsing principals file (expected JSON: { \"<token>\": [\"role\"] })")
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

    let principals = load_principals(cell.principals.as_deref())?;
    if !cell.def.access.roles.is_empty() && principals.is_empty() {
        tracing::warn!(
            "access.roles set but principals file is empty; all authorized requests will be denied"
        );
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
        .and_then(|raw| serde_json::from_str::<crate::manifest::Published>(&raw).ok())
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

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn export() -> Export {
        let mut schema = IndexMap::new();
        schema.insert("order_date".to_string(), "date".to_string());
        schema.insert("region".to_string(), "string".to_string());
        schema.insert("revenue".to_string(), "decimal".to_string());
        Export {
            name: "orders_daily".to_string(),
            version: "2.1.0".to_string(),
            source: Some("orders_daily".to_string()),
            grain: vec!["order_date".to_string(), "region".to_string()],
            schema,
            freshness: None,
            visibility: Visibility::Discoverable,
            contract: Contract::Experimental,
        }
    }

    #[test]
    fn selects_declared_columns_in_order() {
        let sql = build_query(&export(), &HashMap::new(), None);
        assert!(
            sql.contains("SELECT order_date, region, revenue FROM orders_daily"),
            "got: {sql}"
        );
    }

    #[test]
    fn empty_schema_selects_star() {
        let mut e = export();
        e.schema = IndexMap::new();
        let sql = build_query(&e, &HashMap::new(), None);
        assert!(sql.contains("SELECT * FROM orders_daily"), "got: {sql}");
    }

    #[test]
    fn defaults_to_limit_100_offset_0_and_no_where() {
        let sql = build_query(&export(), &HashMap::new(), None);
        assert!(sql.contains("LIMIT 100 OFFSET 0"), "got: {sql}");
        assert!(!sql.contains("WHERE"), "got: {sql}");
    }

    #[test]
    fn grain_params_become_escaped_where_filters() {
        let mut params = HashMap::new();
        params.insert("order_date".to_string(), "2026-06-01".to_string());
        params.insert("region".to_string(), "us-east".to_string());
        let sql = build_query(&export(), &params, None);
        assert!(sql.contains("order_date = '2026-06-01'"), "got: {sql}");
        assert!(sql.contains("region = 'us-east'"), "got: {sql}");
        assert!(sql.contains(" WHERE "), "got: {sql}");
    }

    #[test]
    fn grain_filter_values_are_quote_escaped() {
        let mut params = HashMap::new();
        params.insert("region".to_string(), "o'brien".to_string());
        let sql = build_query(&export(), &params, None);
        // Single quotes doubled — no SQL injection through grain values.
        assert!(sql.contains("region = 'o''brien'"), "got: {sql}");
    }

    #[test]
    fn non_grain_params_are_ignored_in_where() {
        let mut params = HashMap::new();
        params.insert("revenue".to_string(), "999".to_string()); // declared but not grain
        params.insert("evil".to_string(), "1; DROP TABLE x".to_string());
        let sql = build_query(&export(), &params, None);
        assert!(!sql.contains("WHERE"), "got: {sql}");
        assert!(!sql.contains("DROP TABLE"), "got: {sql}");
    }

    #[test]
    fn limit_is_capped_and_offset_passed_through() {
        let mut params = HashMap::new();
        params.insert("limit".to_string(), "999999".to_string());
        params.insert("offset".to_string(), "50".to_string());
        let sql = build_query(&export(), &params, None);
        assert!(
            sql.contains(&format!("LIMIT {MAX_LIMIT} OFFSET 50")),
            "got: {sql}"
        );
    }

    #[test]
    fn invalid_limit_falls_back_to_default() {
        let mut params = HashMap::new();
        params.insert("limit".to_string(), "not-a-number".to_string());
        let sql = build_query(&export(), &params, None);
        assert!(
            sql.contains(&format!("LIMIT {DEFAULT_LIMIT}")),
            "got: {sql}"
        );
    }

    #[test]
    fn snapshot_pins_a_version() {
        let sql = build_query(&export(), &HashMap::new(), Some(42));
        assert!(
            sql.contains("orders_daily AT (VERSION => 42)"),
            "got: {sql}"
        );
    }

    #[test]
    fn no_snapshot_means_no_version_clause() {
        let sql = build_query(&export(), &HashMap::new(), None);
        assert!(!sql.contains("VERSION =>"), "got: {sql}");
    }

    // §8 companion hardening: load_principals must fail loud, not swallow errors
    // into an all-deny map.

    #[test]
    fn load_principals_none_path_is_empty_ok() {
        // No `principals:` configured = legitimately empty (open endpoint, gated upstream).
        let map = load_principals(None).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn load_principals_missing_file_errors() {
        let err = load_principals(Some("/datamk/definitely/missing/principals.json"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("reading principals file"), "unexpected: {err}");
    }

    #[test]
    fn load_principals_malformed_json_errors() {
        let dir = std::env::temp_dir();
        let path = dir.join("datamk_test_bad_principals.json");
        std::fs::write(&path, "{ not valid json").unwrap();
        let err = load_principals(Some(path.to_str().unwrap()))
            .unwrap_err()
            .to_string();
        let _ = std::fs::remove_file(&path);
        assert!(err.contains("parsing principals file"), "unexpected: {err}");
    }

    #[test]
    fn load_principals_valid_file_parses() {
        let dir = std::env::temp_dir();
        let path = dir.join("datamk_test_good_principals.json");
        std::fs::write(&path, r#"{ "tok": ["analyst"] }"#).unwrap();
        let map = load_principals(Some(path.to_str().unwrap())).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(map.get("tok").unwrap(), &vec!["analyst".to_string()]);
    }

    // §9: request queries must stay in autocommit. An explicit BEGIN would pin the
    // catalog attach and block the Builder's commit. Guard the built query string.
    #[test]
    fn built_query_never_opens_a_transaction() {
        let mut params = HashMap::new();
        params.insert("region".to_string(), "us-east".to_string());
        let sql = build_query(&export(), &params, Some(7));
        let upper = sql.to_uppercase();
        assert!(!upper.contains("BEGIN"), "query must not BEGIN: {sql}");
        assert!(!upper.contains("COMMIT"), "query must not COMMIT: {sql}");
    }
}
