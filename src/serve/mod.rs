mod openapi;

use anyhow::{Context, Result};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::config::{Contract, Export};
use crate::engine;

pub(crate) const MAX_LIMIT: usize = 1000;
pub(crate) const DEFAULT_LIMIT: usize = 100;
/// Cap on `offset` (ADR 0012 §7): parsed-but-uncapped offsets let a paginating
/// caller walk arbitrarily deep scans. Requests beyond the cap are rejected
/// (400), never silently clamped — a clamped offset would return a page the
/// caller didn't ask for, which reads as data, not as an error.
pub(crate) const MAX_OFFSET: usize = 1_000_000;
/// Default for `--max-concurrency`: requests over this are shed with 503
/// rather than queued without bound — agents fan out and retry tirelessly
/// (ADR 0012 §7). Real per-client rate limiting belongs to a reverse proxy.
pub const DEFAULT_MAX_CONCURRENCY: usize = 64;

struct AppState {
    /// The opened cell (connection + published-mode state). Behind a Mutex so
    /// the poller can swap in a freshly fetched execution (ADR 0004 §6): the
    /// swap is an assignment under the lock, an in-flight request finishes on
    /// the cell it started with, and dropping the old cell reclaims its local
    /// scratch (artifact file) — resident generations are bounded at two.
    cell: Mutex<engine::Cell>,
    /// route key (`name@major`) -> export — every discoverable export,
    /// including a `materialize: never` one (issue #6): the dispatch map
    /// needs to tell "no such export" (404, unknown route) apart from "this
    /// export exists but is not routed here" (404, `never_backed_routes`,
    /// after `authorize()`).
    routes: HashMap<String, Export>,
    /// route keys sourced from a `materialize: never` table (issue #6): not
    /// mounted, described only. Checked post-`authorize()` in
    /// `serve_export_inner` — a pre-auth check here would let an
    /// unauthenticated caller enumerate which exports are virtual.
    never_backed_routes: HashSet<String>,
    /// route key -> pinned snapshot id (from the release manifest)
    published: BTreeMap<String, i64>,
    openapi: serde_json::Value,
    cell_name: String,
    /// Authorization policy (default-deny).
    shareable: bool,
    allowed_roles: Vec<String>,
    /// bearer token -> roles
    principals: HashMap<String, Vec<String>>,
    /// Currently served execution number (published mode; 0 = direct mode).
    execution: std::sync::atomic::AtomicU64,
    /// Poll telemetry — what makes bounded staleness visible (ADR 0004 §6).
    freshness: Mutex<Freshness>,
    /// The declared region of the context document (ADR 0012), precomputed —
    /// the interface never changes for the lifetime of the process.
    declared: crate::context::Declared,
    /// The interface digest: `/context`'s `ETag`, `/openapi.json`'s
    /// `info.version`, and the `X-Datamk-Context-Digest` back-link header.
    digest: String,
    /// Whether this server runs on a direct-attach (local catalog) profile —
    /// pinless, so its context document is a draft by definition (ADR 0012 §4).
    direct_attach: bool,
    /// The served execution's run summary, fetched by the poller on every
    /// tick (ADR 0012 §5) — decoupled from the swap check, so a summary that
    /// lands after `LATEST` advances is never orphaned. Handlers touch no
    /// store and no DuckDB; they read this cache only.
    run_summary: Mutex<Option<crate::engine::run_summary::RunSummary>>,
    /// Newest lake snapshot time, cached at open and at swap — never queried
    /// on the request path.
    data_as_of: Mutex<Option<String>>,
    /// The sorted, **snapshot-backed** route list (issue #6: `mounted_routes`
    /// applied to the full discoverable list) — kept so the poller can
    /// re-run the swap-time probes against a fresh cell. Never-backed
    /// exports are skipped: their `source_object()` is not a lake table, so
    /// there is nothing here to probe (see `mounted_routes`'s doc comment).
    route_list: Vec<(String, Export)>,
    /// Whether the data routes are mounted (`false` under `--no-data`,
    /// ADR 0012 §4). Drives `data.served_here` honestly, by construction.
    data_mounted: bool,
    /// Profile-declared locations rows actually live when not served here.
    channels: Vec<String>,
    /// Swap-time probe results (ADR 0012 §5): computed at open and at swap
    /// on the poller thread, never on the request path; omitted pieces stay
    /// omitted rather than blocking serving.
    probes: Mutex<indexmap::IndexMap<String, crate::context::ExportProbe>>,
}

#[derive(Default, Clone)]
struct Freshness {
    /// Newest execution the poller has seen `LATEST` name.
    latest_seen: u64,
    /// Unix seconds of the last successful `LATEST` poll.
    last_ok_poll_unix: Option<u64>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
pub async fn run(
    file: &Path,
    profile: &str,
    port: u16,
    poll_interval: u64,
    max_concurrency: usize,
    no_data: bool,
) -> Result<()> {
    let cell = engine::open(file, profile, /* read_only */ true)?;
    let (state, store) = build_state(cell, no_data)?;

    // Initial run-summary fetch (ADR 0012 §5): serve startup already talks to
    // the store (`engine::open` downloaded the artifact), so one more GET here
    // is the same trust and cost — and it means `/context` is verified from
    // the first request, not after the first poll tick.
    if let Some(store) = &store {
        fetch_run_summary(&state, store);
    }

    if let Some(store) = store {
        spawn_poller(
            state.clone(),
            store,
            file.to_path_buf(),
            profile.to_string(),
            poll_interval.max(1),
        );
    }

    let app = app(state, max_concurrency);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(%addr, "serving cell");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Build the serving state from an opened cell. Split from `run` so the
/// in-process smoke tests can stand up the exact router `serve` binds,
/// without a socket. Returns the store handle separately (published mode)
/// so `run` can hand it to the poller.
fn build_state(
    cell: engine::Cell,
    no_data: bool,
) -> Result<(Arc<AppState>, Option<Arc<crate::store::Store>>)> {
    let published = load_published(&cell.dir);
    let data_mounted = !no_data;

    // The one visibility-filtered route list (ADR 0012 §4): the router's
    // dispatch map, the OpenAPI doc, and the context document all derive
    // from this single call — never three independent predicates. Includes
    // `materialize: never` exports (issue #6) — `declared` is unconditional
    // (datamk owns the contract regardless of who owns the rows); `mounted`
    // is the snapshot-backed subset actually routed over HTTP.
    let all_routes = crate::context::discoverable_routes(&cell.def)?;
    let never_tables = crate::config::never_backed_tables(&cell.transforms);
    let mounted = crate::context::mounted_routes(&all_routes, &never_tables);
    let never_backed_routes: HashSet<String> = all_routes
        .iter()
        .filter(|(_, e)| never_tables.contains(e.source_object()))
        .map(|(route, _)| route.clone())
        .collect();
    let routes: HashMap<String, Export> = all_routes.iter().cloned().collect();
    // Under --no-data the query block is omitted (it describes HTTP
    // affordances that do not exist there — ADR 0012 §4); per-export it's
    // also omitted for a never-backed export regardless of --no-data.
    let declared = crate::context::declared(
        &cell.def,
        &all_routes,
        /* with_query */ data_mounted,
        &never_tables,
    );
    let data = crate::context::DataBlock {
        served_here: data_mounted,
        channels: cell.channels.clone(),
    };
    let digest = crate::context::interface_digest(&cell.def.cell, &declared, &data);

    let principals = load_principals(cell.principals.as_deref())?;
    if !cell.def.access.roles.is_empty() && principals.is_empty() {
        tracing::warn!(
            "access.roles set but principals file is empty; all authorized requests will be denied"
        );
    }

    // Published mode (ADR 0004 §6): keep an independent handle to the store so
    // the poller never holds the cell lock across a network call.
    let store = cell.published.as_ref().map(|p| p.store.clone());
    let execution = cell
        .published
        .as_ref()
        .and_then(|p| p.execution)
        .unwrap_or(0);

    let data_as_of = query_data_as_of(&cell.conn);
    let direct_attach = cell.published.is_none();
    // The open-time probe run (ADR 0012 §5) — same measurements the poller
    // repeats at every swap. `--no-data` withholds the row-derived `values`
    // lists; coverage and counts stay (aggregates that name no entity).
    // Never-backed exports (issue #6) are excluded by construction: `mounted`
    // already dropped them, and their `source_object()` names no lake
    // relation to probe.
    let probes = probe_exports(
        &cell.conn,
        &mounted,
        &published,
        /* include_values */ data_mounted,
    );

    let state = Arc::new(AppState {
        // Under --no-data the OpenAPI paths are empty: the spec describes
        // the callable HTTP surface, and the data routes are not mounted.
        // Never-backed exports are always excluded (issue #6) — `mounted`
        // already dropped them regardless of --no-data.
        openapi: openapi::generate(
            &cell.def.cell,
            cell.def.description.as_deref(),
            if data_mounted { &mounted } else { &[] },
            &digest,
        ),
        cell_name: cell.def.cell.clone(),
        routes,
        never_backed_routes,
        published,
        shareable: cell.def.access.shareable,
        allowed_roles: cell.def.access.roles.clone(),
        principals,
        execution: std::sync::atomic::AtomicU64::new(execution),
        freshness: Mutex::new(Freshness {
            latest_seen: execution,
            last_ok_poll_unix: store.as_ref().map(|_| unix_now()),
        }),
        declared,
        digest,
        direct_attach,
        run_summary: Mutex::new(None),
        data_as_of: Mutex::new(data_as_of),
        route_list: mounted,
        data_mounted,
        channels: cell.channels.clone(),
        probes: Mutex::new(probes),
        cell: Mutex::new(cell),
    });
    Ok((state, store))
}

/// The swap-time probe (ADR 0012 §5): per export, the row count, min/max of
/// date/timestamp-typed grain columns, distinct values of string grain
/// columns (`LIMIT 51` — ≤50 listed as complete, more omitted as
/// incomplete), and one real row's grain values joined into
/// `example_request`. Runs on the freshly opened connection at open and at
/// swap — never the request path — against the same rows each route serves
/// (the pinned snapshot for supported routes). Every piece is best-effort:
/// a failure omits that piece and never blocks serving.
fn probe_exports(
    conn: &duckdb::Connection,
    route_list: &[(String, Export)],
    published: &BTreeMap<String, i64>,
    include_values: bool,
) -> indexmap::IndexMap<String, crate::context::ExportProbe> {
    use crate::context::{ColumnCoverage, ColumnValues, ExportProbe};

    let mut out = indexmap::IndexMap::new();
    for (route, export) in route_list {
        let snapshot = if export.contract == Contract::Supported {
            published.get(route).copied()
        } else {
            None
        };
        let at = match snapshot {
            Some(id) => format!(" AT (VERSION => {id})"),
            None => String::new(),
        };
        let source = export.source_object();
        let mut probe = ExportProbe {
            rows: conn
                .prepare(&format!("SELECT count(*) FROM {source}{at}"))
                .and_then(|mut s| s.query_row([], |r| r.get::<_, i64>(0)))
                .ok(),
            ..ExportProbe::default()
        };

        for g in &export.grain {
            let Some(spec) = export.schema.get(g) else {
                continue; // no declared type — no typed probe to run
            };
            match spec.ty.to_lowercase().as_str() {
                "date" | "timestamp" => {
                    let minmax = conn
                        .prepare(&format!(
                            "SELECT CAST(min({g}) AS VARCHAR), CAST(max({g}) AS VARCHAR) \
                             FROM {source}{at}"
                        ))
                        .and_then(|mut s| {
                            s.query_row([], |r| {
                                Ok((
                                    r.get::<_, Option<String>>(0)?,
                                    r.get::<_, Option<String>>(1)?,
                                ))
                            })
                        })
                        .ok();
                    if let Some((Some(min), Some(max))) = minmax {
                        probe
                            .coverage
                            .insert(g.clone(), ColumnCoverage { min, max });
                    }
                }
                "string" | "varchar" | "text" if include_values => {
                    let vals: Option<Vec<String>> = conn
                        .prepare(&format!(
                            "SELECT DISTINCT CAST({g} AS VARCHAR) FROM {source}{at} \
                             WHERE {g} IS NOT NULL ORDER BY 1 LIMIT 51"
                        ))
                        .and_then(|mut s| {
                            let rows = s.query_map([], |r| r.get::<_, String>(0))?;
                            rows.collect()
                        })
                        .ok();
                    if let Some(vals) = vals {
                        let complete = vals.len() <= 50;
                        probe.values.insert(
                            g.clone(),
                            ColumnValues {
                                values: complete.then_some(vals),
                                complete,
                            },
                        );
                    }
                }
                _ => {}
            }
        }

        // One REAL row's grain values, drawn jointly (never composed from the
        // per-column values, which can name a combination that co-occurs
        // nowhere). ORDER BY the grain so the example is stable across polls.
        if include_values && !export.grain.is_empty() {
            let cols = export
                .grain
                .iter()
                .map(|g| format!("CAST({g} AS VARCHAR)"))
                .collect::<Vec<_>>()
                .join(", ");
            let order = export.grain.join(", ");
            let row: Option<Vec<Option<String>>> = conn
                .prepare(&format!(
                    "SELECT {cols} FROM {source}{at} ORDER BY {order} LIMIT 1"
                ))
                .and_then(|mut s| {
                    s.query_row([], |r| {
                        (0..export.grain.len())
                            .map(|i| r.get::<_, Option<String>>(i))
                            .collect()
                    })
                })
                .ok();
            // Emitted only when every grain column got a value — never a
            // placeholder, which an agent pastes literally.
            if let Some(row) = row {
                if row.iter().all(Option::is_some) {
                    let params = export
                        .grain
                        .iter()
                        .zip(&row)
                        .map(|(g, v)| format!("{g}={}", percent_encode(v.as_deref().unwrap_or(""))))
                        .collect::<Vec<_>>()
                        .join("&");
                    probe.example_request = Some(format!("/{route}?{params}&limit=10"));
                }
            }
        }

        out.insert(route.clone(), probe);
    }
    out
}

/// Minimal query-value percent-encoding: unreserved characters pass through,
/// everything else is %XX-escaped — enough for grain values in an example
/// URL, with no dependency.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Newest lake snapshot time — when the data actually last moved. Best-effort
/// and never on the request path (ADR 0012 §5): read once at open and again
/// at swap; a failure is `None`, never a fabricated value.
fn query_data_as_of(conn: &duckdb::Connection) -> Option<String> {
    conn.prepare("SELECT CAST(max(snapshot_time) AS VARCHAR) FROM ducklake_snapshots('lake')")
        .and_then(|mut s| s.query_row([], |r| r.get::<_, Option<String>>(0)))
        .ok()
        .flatten()
}

/// Fetch the served execution's run summary into the state cache (ADR 0012
/// §5). Skips the GET only when the cache already holds the summary for the
/// currently served execution — summaries are immutable once written, and a
/// miss (summary not yet landed, or store hiccup) is retried on every tick,
/// which is the property the ADR requires: nothing is gated on the swap
/// branch, so a summary that lands late is never orphaned.
fn fetch_run_summary(state: &AppState, store: &crate::store::Store) {
    let served = state.execution.load(std::sync::atomic::Ordering::Relaxed);
    if served == 0 {
        return;
    }
    {
        let cached = state
            .run_summary
            .lock()
            .expect("run_summary mutex poisoned");
        if cached.as_ref().is_some_and(|s| s.execution == served) {
            return;
        }
    }
    match store.get(&crate::store::run_summary_key(served)) {
        Ok(Some(bytes)) => match serde_json::from_slice(&bytes) {
            Ok(summary) => {
                *state
                    .run_summary
                    .lock()
                    .expect("run_summary mutex poisoned") = Some(summary);
            }
            Err(e) => {
                tracing::warn!(error = %e, execution = served, "run summary unparseable; context provenance stays absent");
            }
        },
        Ok(None) => {} // not landed yet — retried next tick
        Err(e) => {
            tracing::warn!(error = %e, execution = served, "run summary fetch failed; retrying next tick");
        }
    }
}

/// The router `serve` binds, wrapped in the throttle stack (ADR 0012 §7):
/// a global concurrency cap with load-shed. Requests over the cap get an
/// immediate 503 instead of queueing without bound — the clone-shared
/// semaphore makes the cap global across connections. Per-client fairness
/// is a reverse proxy's job (docs/guides/serving.md), not this socket's.
fn app(state: Arc<AppState>, max_concurrency: usize) -> Router {
    // `/context` replaces the old `/interface` stub (ADR 0012 §4): renamed,
    // not duplicated — one document, one route, and the old name 404s (same
    // rule as unmounted data routes: no door, no 403). No reserved-name
    // collision: export routes always carry the major (`name@major`), so an
    // export named `context` serves at `/context@1`.
    Router::new()
        .route("/", get(health))
        .route("/context", get(context_doc))
        .route("/openapi.json", get(openapi_doc))
        .route("/:route", get(serve_export))
        .layer(
            tower::ServiceBuilder::new()
                .layer(axum::error_handling::HandleErrorLayer::new(
                    |_: tower::BoxError| async {
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            "server at capacity — retry with backoff",
                        )
                    },
                ))
                .load_shed()
                .concurrency_limit(max_concurrency.max(1)),
        )
        .with_state(state)
}

/// The fetch-and-swap poller (ADR 0004 §6): re-read `LATEST` every interval;
/// on a new execution, open a fresh cell (which downloads the artifact and
/// attaches a private local copy) and swap it in under the lock. A plain OS
/// thread: the store's surface is sync, and the swap is an assignment.
///
/// A wedged poll (store unreachable) keeps serving last-good data; the
/// freshness telemetry on `/context` is what stops that from being
/// invisible.
fn spawn_poller(
    state: Arc<AppState>,
    store: Arc<crate::store::Store>,
    file: std::path::PathBuf,
    profile: String,
    interval_secs: u64,
) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(interval_secs));

        let latest = match store.latest() {
            Ok(Some(n)) => n,
            Ok(None) => {
                tracing::warn!("LATEST pointer disappeared; keeping last-good catalog");
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "polling LATEST failed; keeping last-good catalog");
                continue;
            }
        };

        {
            let mut f = state.freshness.lock().expect("freshness mutex poisoned");
            f.latest_seen = latest;
            f.last_ok_poll_unix = Some(unix_now());
        }

        let served = state.execution.load(std::sync::atomic::Ordering::Relaxed);
        if latest != served {
            // A rollback (§9) moves LATEST backwards; advance or retreat alike,
            // serve what the pointer names.
            match engine::open(&file, &profile, /* read_only */ true) {
                Ok(new_cell) => {
                    let n = new_cell
                        .published
                        .as_ref()
                        .and_then(|p| p.execution)
                        .unwrap_or(0);
                    // Read the new lake's snapshot time and re-run the probes
                    // before the swap — off the request path, on the
                    // connection no request holds yet (ADR 0012 §5).
                    let as_of = query_data_as_of(&new_cell.conn);
                    let published = load_published(&new_cell.dir);
                    let probes = probe_exports(
                        &new_cell.conn,
                        &state.route_list,
                        &published,
                        state.data_mounted,
                    );
                    // Swap under the lock: in-flight requests finish on the old
                    // cell first (they hold the lock for the query's duration);
                    // dropping it reclaims its scratch artifact.
                    *state.cell.lock().expect("cell mutex poisoned") = new_cell;
                    state
                        .execution
                        .store(n, std::sync::atomic::Ordering::Relaxed);
                    *state.data_as_of.lock().expect("data_as_of mutex poisoned") = as_of;
                    *state.probes.lock().expect("probes mutex poisoned") = probes;
                    tracing::info!(execution = n, "swapped to newly published execution");
                }
                Err(e) => {
                    tracing::warn!(error = %e, execution = latest,
                        "failed to open newly published execution; keeping last-good catalog");
                }
            }
        }

        // ADR 0012 §5: the run-summary fetch rides every poll tick, never the
        // swap branch — the summary is written after `publish_execution`
        // returns, so gating it on the swap would orphan any summary that
        // lands after `LATEST` advances.
        fetch_run_summary(&state, &store);
    });
}

fn load_published(dir: &Path) -> BTreeMap<String, i64> {
    let path = dir.join(".cell").join("published.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<crate::manifest::Published>(&raw).ok())
        .map(|p| p.routes)
        .unwrap_or_default()
}

/// Pre-authorize liveness route (`/`). Carries the served execution number —
/// a low-sensitivity monotonic counter that lets a smoke test confirm a swap
/// happened (ADR 0004 §6). Full freshness detail stays behind auth.
async fn health(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let mut body = serde_json::json!({ "cell": s.cell_name, "status": "ok" });
    let execution = s.execution.load(std::sync::atomic::Ordering::Relaxed);
    if execution > 0 {
        body["execution"] = execution.into();
    }
    Json(body)
}

/// `GET /context` (ADR 0012): the cell's interface made machine-readable.
/// Same auth tier as the data — the document is the map (grain, columns,
/// upstream refs); no lower "docs" tier, no pre-auth serving. Handlers touch
/// no store and no DuckDB: everything here reads precomputed state and the
/// poller-maintained caches.
async fn context_doc(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = authorize(&s, &headers) {
        return resp;
    }

    let etag = format!("\"{}\"", s.digest);
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag)
    {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }

    let execution = s.execution.load(std::sync::atomic::Ordering::Relaxed);

    // Provenance only when the cached summary describes the execution being
    // served right now — a summary from a previous execution paired with
    // fresher rows would be a claim mislabeled as a measurement.
    let provenance = s
        .run_summary
        .lock()
        .expect("run_summary mutex poisoned")
        .as_ref()
        .filter(|summary| summary.execution == execution)
        .map(|summary| {
            crate::context::provenance_from(
                summary,
                s.data_as_of
                    .lock()
                    .expect("data_as_of mutex poisoned")
                    .clone(),
            )
        });

    // Poll telemetry (ADR 0004 §6, carried over from the old `/interface`):
    // published mode only — direct mode has no poller and no staleness bound.
    let freshness = (execution > 0).then(|| {
        let f = s
            .freshness
            .lock()
            .expect("freshness mutex poisoned")
            .clone();
        crate::context::FreshnessBlock {
            serving_execution: execution,
            latest_seen: f.latest_seen,
            last_successful_poll_age_seconds: f
                .last_ok_poll_unix
                .map(|t| unix_now().saturating_sub(t)),
        }
    });

    let probes = s.probes.lock().expect("probes mutex poisoned").clone();
    let mut doc = crate::context::assemble(
        s.cell_name.clone(),
        s.declared.clone(),
        provenance,
        // Issue #6: `observed.source_check` is wired for the portable
        // `datamk verify` -> `datamk context` CI path only in this slice —
        // the Server stays credential-light and never performs a live
        // warehouse check itself (Q1's whole point). Reading a persisted
        // `.cell/source_check.json` here to surface it on the hosted
        // `/context` too is a reasonable follow-up, deliberately deferred.
        /* source_check */
        None,
        freshness,
        probes,
        /* served_here */ s.data_mounted,
        s.channels.clone(),
        s.direct_attach,
    );
    if !s.data_mounted {
        // The same engine-emitted sentence the unmounted routes' 404 body
        // carries (ADR 0012 §4).
        doc.notes.push(crate::context::NOTE_NO_DATA.to_string());
    }
    (StatusCode::OK, [(header::ETAG, etag)], Json(doc)).into_response()
}

/// The back-link headers every data-route response carries (ADR 0012 §4) —
/// 200 and 404 alike, because a wrong guess is exactly when the map matters.
/// `X-Datamk-Context-Digest`, not `ETag`: on a data response the ETag tags
/// the rows, and overloading it breaks `If-None-Match`. `X-Datamk-Execution`
/// mirrors what the pre-auth health route already exposes. Headers only —
/// the row body stays a bare JSON array.
fn with_context_headers(s: &AppState, mut resp: Response) -> Response {
    use axum::http::HeaderValue;
    let h = resp.headers_mut();
    h.insert(
        header::LINK,
        HeaderValue::from_static("</context>; rel=\"describedby\""),
    );
    if let Ok(v) = HeaderValue::from_str(&s.digest) {
        h.insert("x-datamk-context-digest", v);
    }
    let execution = s.execution.load(std::sync::atomic::Ordering::Relaxed);
    if execution > 0 {
        if let Ok(v) = HeaderValue::from_str(&execution.to_string()) {
            h.insert("x-datamk-execution", v);
        }
    }
    resp
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
    // --no-data (ADR 0012 §4): the data routes are simply not mounted.
    // Unmounted routes return 404, not 403 — a 403 promises a door that
    // exists — and the body carries the same engine-emitted sentence as the
    // document's notes. No auth gate: there is no door to guard.
    if !s.data_mounted {
        return with_context_headers(
            &s,
            (StatusCode::NOT_FOUND, crate::context::NOTE_NO_DATA).into_response(),
        );
    }
    if let Err(resp) = authorize(&s, &headers) {
        return resp;
    }
    // Every post-auth response — 200, 400, 404, 500 — carries the context
    // back-link headers (ADR 0012 §4): a wrong guess is exactly when the map
    // matters.
    with_context_headers(&s, serve_export_inner(&s, route, params).await)
}

async fn serve_export_inner(
    s: &Arc<AppState>,
    route: String,
    params: HashMap<String, String>,
) -> Response {
    let export = match s.routes.get(&route) {
        Some(e) => e.clone(),
        None => return (StatusCode::NOT_FOUND, format!("no export '{route}'")).into_response(),
    };

    // issue #6: this export exists and is declared (it's in `s.routes`), but
    // its transform is `materialize: never` — no lake table backs it, so
    // there is no data route to serve. Runs post-`authorize()` (the caller
    // of this function already checked): a pre-auth 404 here, unlike the
    // cell-wide --no-data check above, would let an unauthenticated caller
    // enumerate which exports are virtual one route at a time.
    if s.never_backed_routes.contains(&route) {
        return (
            StatusCode::NOT_FOUND,
            crate::context::note_never_backed(&route),
        )
            .into_response();
    }

    // ADR 0012 §7: unknown or invalid query params are a 400, never silently
    // dropped — an ignored `?revenue=999` returns unfiltered rows the caller
    // will confidently read as a filtered subset.
    let read = match validate_params(&export, &params) {
        Ok(r) => r,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };

    // Supported contracts serve a pinned snapshot; experimental tracks latest.
    let snapshot = if export.contract == Contract::Supported {
        s.published.get(&route).copied()
    } else {
        None
    };

    let sql = build_query(&export, &read, snapshot);

    let s2 = s.clone();
    let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<String>> {
        // Holding the cell lock for the query's duration is what guarantees a
        // request never spans two catalogs across a poller swap (ADR 0004 §6).
        let cell = s2.cell.lock().expect("cell mutex poisoned");
        run_json_query(&cell.conn, &sql)
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

/// A request's validated read parameters. Filters carry the *declared grain
/// order* regardless of the order params arrived in, so the emitted WHERE is
/// deterministic for a given filter set.
#[derive(Debug)]
struct ReadParams {
    /// grain column -> escaped-later value, in declared grain order.
    filters: Vec<(String, String)>,
    limit: usize,
    offset: usize,
}

/// Reject anything the closed query grammar doesn't accept (ADR 0012 §7):
/// unknown parameter names (400, not silently ignored), non-integer
/// `limit`/`offset`, and an offset beyond `MAX_OFFSET`. An over-`MAX_LIMIT`
/// limit is clamped, not rejected — the caps ship in the served affordances,
/// and a clamped page is still exactly the page asked for, just shorter.
fn validate_params(
    export: &Export,
    params: &HashMap<String, String>,
) -> std::result::Result<ReadParams, String> {
    for k in params.keys() {
        if k != "limit" && k != "offset" && !export.grain.iter().any(|g| g == k) {
            let grain = if export.grain.is_empty() {
                "no grain filters".to_string()
            } else {
                format!("grain filters ({})", export.grain.join(", "))
            };
            return Err(format!(
                "unknown query parameter '{k}' — this export accepts {grain}, plus `limit` \
                 and `offset` (exact equality only; no ranges, no operators, no non-grain \
                 columns)"
            ));
        }
    }

    let limit = match params.get("limit") {
        None => DEFAULT_LIMIT,
        Some(v) => v
            .parse::<usize>()
            .map_err(|_| {
                format!("`limit` must be an integer between 0 and {MAX_LIMIT}, got '{v}'")
            })?
            .min(MAX_LIMIT),
    };
    let offset = match params.get("offset") {
        None => 0,
        Some(v) => {
            let n = v.parse::<usize>().map_err(|_| {
                format!("`offset` must be an integer between 0 and {MAX_OFFSET}, got '{v}'")
            })?;
            if n > MAX_OFFSET {
                return Err(format!(
                    "`offset` {n} exceeds the maximum {MAX_OFFSET} — filter by grain columns \
                     instead of paginating this deep"
                ));
            }
            n
        }
    };

    let filters = export
        .grain
        .iter()
        .filter_map(|g| params.get(g).map(|v| (g.clone(), v.clone())))
        .collect();

    Ok(ReadParams {
        filters,
        limit,
        offset,
    })
}

/// Build the read query for an export. Only declared columns and grain columns
/// reach the SQL (both come from the cell definition, not user input); grain
/// filter *values* are escaped. The subquery is ordered before LIMIT/OFFSET
/// (ADR 0012 §7): limit/offset over nondeterministic order silently skips and
/// double-counts rows for any paginating caller. Sort key: the declared grain
/// (unique, per `verify`); a grainless export falls back to DuckDB's
/// `ORDER BY ALL` — every column, still deterministic.
fn build_query(export: &Export, read: &ReadParams, snapshot: Option<i64>) -> String {
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

    let wheres: Vec<String> = read
        .filters
        .iter()
        .map(|(g, v)| format!("{g} = '{}'", v.replace('\'', "''")))
        .collect();
    let where_clause = if wheres.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", wheres.join(" AND "))
    };

    let order_by = if export.grain.is_empty() {
        "ALL".to_string()
    } else {
        export.grain.join(", ")
    };

    format!(
        "SELECT to_json(t) AS j FROM \
         (SELECT {cols} FROM {source}{at}{where_clause} ORDER BY {order_by} \
         LIMIT {limit} OFFSET {offset}) t",
        limit = read.limit,
        offset = read.offset,
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
    use crate::config::Visibility;
    use indexmap::IndexMap;

    fn export() -> Export {
        let mut schema = IndexMap::new();
        schema.insert(
            "order_date".to_string(),
            crate::config::ColumnSpec::bare("date"),
        );
        schema.insert(
            "region".to_string(),
            crate::config::ColumnSpec::bare("string"),
        );
        schema.insert(
            "revenue".to_string(),
            crate::config::ColumnSpec::bare("decimal"),
        );
        Export {
            name: "orders_daily".to_string(),
            version: "2.1.0".to_string(),
            source: Some("orders_daily".to_string()),
            description: None,
            grain: vec!["order_date".to_string(), "region".to_string()],
            schema,
            freshness: None,
            visibility: Visibility::Discoverable,
            contract: Contract::Experimental,
        }
    }

    /// validate-then-build, exactly as `serve_export` composes them.
    fn q(e: &Export, params: &HashMap<String, String>, snapshot: Option<i64>) -> String {
        build_query(
            e,
            &validate_params(e, params).expect("params must validate"),
            snapshot,
        )
    }

    #[test]
    fn selects_declared_columns_in_order() {
        let sql = q(&export(), &HashMap::new(), None);
        assert!(
            sql.contains("SELECT order_date, region, revenue FROM orders_daily"),
            "got: {sql}"
        );
    }

    #[test]
    fn empty_schema_selects_star() {
        let mut e = export();
        e.schema = IndexMap::new();
        let sql = q(&e, &HashMap::new(), None);
        assert!(sql.contains("SELECT * FROM orders_daily"), "got: {sql}");
    }

    #[test]
    fn defaults_to_limit_100_offset_0_and_no_where() {
        let sql = q(&export(), &HashMap::new(), None);
        assert!(sql.contains("LIMIT 100 OFFSET 0"), "got: {sql}");
        assert!(!sql.contains("WHERE"), "got: {sql}");
    }

    // ADR 0012 §7: pagination must be deterministic — the subquery orders by
    // the declared grain before LIMIT/OFFSET applies.
    #[test]
    fn pagination_orders_by_the_declared_grain() {
        let sql = q(&export(), &HashMap::new(), None);
        assert!(
            sql.contains("ORDER BY order_date, region LIMIT"),
            "got: {sql}"
        );
    }

    #[test]
    fn grainless_export_orders_by_all() {
        let mut e = export();
        e.grain = vec![];
        let sql = q(&e, &HashMap::new(), None);
        assert!(sql.contains("ORDER BY ALL LIMIT"), "got: {sql}");
    }

    #[test]
    fn grain_params_become_escaped_where_filters() {
        let mut params = HashMap::new();
        params.insert("order_date".to_string(), "2026-06-01".to_string());
        params.insert("region".to_string(), "us-east".to_string());
        let sql = q(&export(), &params, None);
        assert!(sql.contains("order_date = '2026-06-01'"), "got: {sql}");
        assert!(sql.contains("region = 'us-east'"), "got: {sql}");
        assert!(sql.contains(" WHERE "), "got: {sql}");
    }

    #[test]
    fn grain_filter_values_are_quote_escaped() {
        let mut params = HashMap::new();
        params.insert("region".to_string(), "o'brien".to_string());
        let sql = q(&export(), &params, None);
        // Single quotes doubled — no SQL injection through grain values.
        assert!(sql.contains("region = 'o''brien'"), "got: {sql}");
    }

    // ADR 0012 §7 (behavior change): a non-grain param used to be silently
    // ignored — unfiltered rows a caller confidently reads as a filtered
    // subset. Now it is rejected before any SQL is built.
    #[test]
    fn non_grain_params_are_rejected() {
        let mut params = HashMap::new();
        params.insert("revenue".to_string(), "999".to_string()); // declared but not grain
        let err = validate_params(&export(), &params).unwrap_err();
        assert!(
            err.contains("unknown query parameter 'revenue'"),
            "got: {err}"
        );
        assert!(err.contains("order_date, region"), "got: {err}");

        let mut params = HashMap::new();
        params.insert("evil".to_string(), "1; DROP TABLE x".to_string());
        let err = validate_params(&export(), &params).unwrap_err();
        assert!(err.contains("unknown query parameter 'evil'"), "got: {err}");
    }

    #[test]
    fn limit_is_capped_and_offset_passed_through() {
        let mut params = HashMap::new();
        params.insert("limit".to_string(), "999999".to_string());
        params.insert("offset".to_string(), "50".to_string());
        let sql = q(&export(), &params, None);
        assert!(
            sql.contains(&format!("LIMIT {MAX_LIMIT} OFFSET 50")),
            "got: {sql}"
        );
    }

    // ADR 0012 §7 (behavior change): an unparseable limit used to silently
    // fall back to the default; now it is a 400-shaped validation error.
    #[test]
    fn invalid_limit_is_rejected() {
        let mut params = HashMap::new();
        params.insert("limit".to_string(), "not-a-number".to_string());
        let err = validate_params(&export(), &params).unwrap_err();
        assert!(err.contains("`limit` must be an integer"), "got: {err}");
    }

    #[test]
    fn offset_beyond_the_cap_is_rejected() {
        let mut params = HashMap::new();
        params.insert("offset".to_string(), (MAX_OFFSET + 1).to_string());
        let err = validate_params(&export(), &params).unwrap_err();
        assert!(err.contains("exceeds the maximum"), "got: {err}");

        let mut params = HashMap::new();
        params.insert("offset".to_string(), MAX_OFFSET.to_string());
        assert_eq!(
            validate_params(&export(), &params).unwrap().offset,
            MAX_OFFSET,
            "the cap itself is legal"
        );
    }

    #[test]
    fn invalid_offset_is_rejected() {
        let mut params = HashMap::new();
        params.insert("offset".to_string(), "-1".to_string());
        let err = validate_params(&export(), &params).unwrap_err();
        assert!(err.contains("`offset` must be an integer"), "got: {err}");
    }

    #[test]
    fn filters_follow_declared_grain_order_not_param_order() {
        let mut params = HashMap::new();
        params.insert("region".to_string(), "us-east".to_string());
        params.insert("order_date".to_string(), "2026-06-01".to_string());
        let read = validate_params(&export(), &params).unwrap();
        let cols: Vec<&str> = read.filters.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(cols, vec!["order_date", "region"]);
    }

    #[test]
    fn snapshot_pins_a_version() {
        let sql = q(&export(), &HashMap::new(), Some(42));
        assert!(
            sql.contains("orders_daily AT (VERSION => 42)"),
            "got: {sql}"
        );
    }

    #[test]
    fn no_snapshot_means_no_version_clause() {
        let sql = q(&export(), &HashMap::new(), None);
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

    // ADR 0012 §7: the fixture test binding the context document's `query`
    // claims to the constants and validation `build_query`/`validate_params`
    // enforce — hand-restated prose beside enforcing code orphans silently on
    // the next change to either. A change to the grammar or to the block
    // fails here, loudly.
    #[test]
    fn context_query_block_claims_match_the_enforced_grammar() {
        let e = export();
        let qb = crate::context::query_block("orders_daily@2", &e);

        // The caps in the block are the caps in the code — same constants.
        assert_eq!(qb.limit_default, DEFAULT_LIMIT);
        assert_eq!(qb.limit_max, MAX_LIMIT);
        assert_eq!(qb.offset_max, MAX_OFFSET);

        // The advertised filters are exactly the accepted filter params:
        // every advertised one validates, any declared-but-unadvertised
        // column is rejected.
        assert_eq!(qb.filters, e.grain);
        for f in &qb.filters {
            let mut p = HashMap::new();
            p.insert(f.clone(), "x".to_string());
            validate_params(&e, &p).expect("advertised filter must validate");
        }
        let unadvertised: Vec<&String> = e
            .schema
            .keys()
            .filter(|c| !qb.filters.contains(c))
            .collect();
        assert!(!unadvertised.is_empty(), "test needs a non-grain column");
        for c in unadvertised {
            let mut p = HashMap::new();
            p.insert(c.clone(), "x".to_string());
            assert!(
                validate_params(&e, &p).is_err(),
                "unadvertised column '{c}' must be rejected"
            );
        }

        // The sample request is a legal sentence in the grammar.
        let (path, query) = qb.sample_request.split_once('?').unwrap();
        assert_eq!(path, "/orders_daily@2");
        let params: HashMap<String, String> = query
            .split('&')
            .map(|kv| kv.split_once('=').unwrap())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        validate_params(&e, &params).expect("sample_request must be a legal call");
    }

    // §9: request queries must stay in autocommit. An explicit BEGIN would pin the
    // catalog attach and block the Builder's commit. Guard the built query string.
    #[test]
    fn built_query_never_opens_a_transaction() {
        let mut params = HashMap::new();
        params.insert("region".to_string(), "us-east".to_string());
        let sql = q(&export(), &params, Some(7));
        let upper = sql.to_uppercase();
        assert!(!upper.contains("BEGIN"), "query must not BEGIN: {sql}");
        assert!(!upper.contains("COMMIT"), "query must not COMMIT: {sql}");
    }
}

/// In-process HTTP smoke tests of the serve surface (ADR 0012 §7) — the first
/// tests that drive the actual router. A real cell (the `init` scaffold, which
/// runs with zero external setup) is built once with `engine::run`, then the
/// exact router `serve` binds is driven with `tower::ServiceExt::oneshot` — no
/// socket, no port. Engine work happens on the test thread (the engine is
/// sync); only the request dispatch needs a runtime.
#[cfg(test)]
mod smoke {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    struct Scaffold {
        dir: std::path::PathBuf,
    }

    impl Drop for Scaffold {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Scaffold + build the init cell once per test binary run.
    fn built_cell() -> &'static Scaffold {
        use std::sync::OnceLock;
        static CELL: OnceLock<Scaffold> = OnceLock::new();
        CELL.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!(
                "datamk-serve-smoke-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            crate::init::run(crate::cli::InitArgs {
                name: "smoke".to_string(),
                path: Some(dir.clone()),
            })
            .expect("init scaffold");
            crate::engine::run(
                &dir.join("cell.yaml"),
                "local",
                None,
                crate::engine::RunOptions::default(),
            )
            .expect("build the scaffold cell");
            Scaffold { dir }
        })
    }

    /// Open the built cell read-only and stand up the exact router `serve`
    /// binds. `mutate` edits the parsed definition before state construction
    /// (how the auth tests flip `shareable`/`roles` without a second cell).
    fn router_mode(no_data: bool, mutate: impl FnOnce(&mut engine::Cell)) -> Router {
        let scaffold = built_cell();
        let mut cell = engine::open(&scaffold.dir.join("cell.yaml"), "local", true)
            .expect("open built cell read-only");
        mutate(&mut cell);
        let (state, _store) = build_state(cell, no_data).expect("build state");
        app(state, DEFAULT_MAX_CONCURRENCY)
    }

    fn router_with(mutate: impl FnOnce(&mut engine::Cell)) -> Router {
        router_mode(false, mutate)
    }

    /// A MIXED cell (issue #6): one materializing export (`stg`) and one
    /// `materialize: never` export (`virtual_pii`) — the shape "described,
    /// not routed" is built to test against, not simulated. Built once
    /// (`engine::run`), same discipline as `built_cell()`.
    fn built_never_cell() -> &'static Scaffold {
        use std::sync::OnceLock;
        static CELL: OnceLock<Scaffold> = OnceLock::new();
        CELL.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!(
                "datamk-serve-smoke-never-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(dir.join("sql")).unwrap();
            std::fs::create_dir_all(dir.join("profiles")).unwrap();
            std::fs::write(
                dir.join("cell.yaml"),
                "cell: virtual_smoke\n\
                 transforms:\n\
                 \x20 - sql/stg.sql\n\
                 \x20 - sql: sql/virtual_pii.sql\n\
                 \x20   materialize: never\n\
                 interface:\n\
                 \x20 - name: stg\n\
                 \x20   version: 1.0.0\n\
                 \x20   description: A materialized export, for contrast with virtual_pii.\n\
                 \x20   grain: [id]\n\
                 \x20   schema:\n\
                 \x20     id: integer\n\
                 \x20     val: string\n\
                 \x20 - name: virtual_pii\n\
                 \x20   version: 1.0.0\n\
                 \x20   description: PII rows datamk verifies but never stores.\n\
                 \x20   grain: [id]\n\
                 \x20   schema:\n\
                 \x20     id: integer\n\
                 \x20     val: string\n\
                 access:\n\
                 \x20 shareable: true\n",
            )
            .unwrap();
            std::fs::write(
                dir.join("sql/stg.sql"),
                "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, val)",
            )
            .unwrap();
            std::fs::write(dir.join("sql/virtual_pii.sql"), "SELECT * FROM stg").unwrap();
            std::fs::write(
                dir.join("profiles/local.yaml"),
                "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
            )
            .unwrap();
            crate::engine::run(
                &dir.join("cell.yaml"),
                "local",
                None,
                crate::engine::RunOptions::default(),
            )
            .expect("build the mixed (materializing + never) scaffold cell");
            Scaffold { dir }
        })
    }

    fn never_router_with(mutate: impl FnOnce(&mut engine::Cell)) -> Router {
        let scaffold = built_never_cell();
        let mut cell = engine::open(&scaffold.dir.join("cell.yaml"), "local", true)
            .expect("open built never-cell read-only");
        mutate(&mut cell);
        let (state, _store) = build_state(cell, /* no_data */ false).expect("build state");
        app(state, DEFAULT_MAX_CONCURRENCY)
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    async fn get(router: &Router, uri: &str, bearer: Option<&str>) -> (StatusCode, String) {
        let mut req = Request::builder().uri(uri);
        if let Some(t) = bearer {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let resp = router
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[test]
    fn open_cell_serves_health_context_openapi_and_rows() {
        let router = router_with(|_| {});
        rt().block_on(async {
            let (status, body) = get(&router, "/", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["cell"], "smoke");
            assert_eq!(v["status"], "ok");

            // The context document (ADR 0012): a direct-attach (local) cell
            // is pinless, therefore draft, with provenance null and the
            // engine-emitted note — never fabricated. The probe measurements
            // (machine facts from the attached catalog) are still present.
            let (status, body) = get(&router, "/context", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["datamk_context"], 1);
            assert_eq!(v["cell"], "smoke");
            assert_eq!(v["status"], "draft");
            assert_eq!(v["grain_verified"], false);
            assert!(v["observed"]["provenance"].is_null(), "{body}");
            assert_eq!(v["declared"]["exports"][0]["route"], "orders_daily@2");
            assert_eq!(
                v["declared"]["exports"][0]["query"]["sample_request"],
                "/orders_daily@2?limit=10"
            );
            assert_eq!(v["data"]["served_here"], true);
            assert_eq!(v["notes"][0], crate::context::NOTE_DIRECT_ATTACH);

            // The old /interface stub is renamed, not duplicated (ADR 0012
            // §4): no door, no 403 — a plain 404.
            let (status, _) = get(&router, "/interface", None).await;
            assert_eq!(status, StatusCode::NOT_FOUND);

            let (status, body) = get(&router, "/openapi.json", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(v["paths"]["/orders_daily@2"].is_object(), "{body}");
            // ADR 0012 §7: the hardcoded "0.0.0" is gone — the version is
            // the interface digest.
            let version = v["info"]["version"].as_str().unwrap();
            assert_ne!(version, "0.0.0");
            assert_eq!(version.len(), 64, "sha256 hex digest: {version}");

            let (status, body) = get(&router, "/orders_daily@2", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let rows: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
            assert!(!rows.is_empty(), "scaffold cell must serve rows");
            assert!(rows[0].get("order_date").is_some(), "{body}");
            assert!(rows[0].get("revenue").is_some(), "{body}");
        });
    }

    /// ADR 0012 §2/§4: the digest is the document's ETag (If-None-Match ->
    /// 304), and every data-route response answers back with the map's
    /// location and digest — 200 and 404 alike.
    #[test]
    fn context_etag_and_data_route_back_links() {
        let router = router_with(|_| {});
        rt().block_on(async {
            let resp = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/context")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let etag = resp
                .headers()
                .get(header::ETAG)
                .expect("context carries an ETag")
                .to_str()
                .unwrap()
                .to_string();

            let resp = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/context")
                        .header(header::IF_NONE_MATCH, &etag)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);

            for uri in ["/orders_daily@2", "/no_such@1"] {
                let resp = router
                    .clone()
                    .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    resp.headers()
                        .get(header::LINK)
                        .and_then(|v| v.to_str().ok()),
                    Some("</context>; rel=\"describedby\""),
                    "{uri} must link back to /context"
                );
                let digest = resp
                    .headers()
                    .get("x-datamk-context-digest")
                    .and_then(|v| v.to_str().ok())
                    .expect("data route carries the context digest");
                assert_eq!(format!("\"{digest}\""), etag, "digest matches the ETag");
            }
        });
    }

    // ADR 0012 §7: limit/offset pagination must stitch together without
    // skipping or double-counting — only true with a deterministic ORDER BY.
    #[test]
    fn pagination_is_deterministic_and_stitches_exactly() {
        let router = router_with(|_| {});
        rt().block_on(async {
            let (_, all) = get(&router, "/orders_daily@2", None).await;
            let all: Vec<serde_json::Value> = serde_json::from_str(&all).unwrap();
            assert!(all.len() >= 3, "scaffold should have several grain rows");

            let mut stitched = Vec::new();
            for page in 0..all.len() {
                let (status, body) = get(
                    &router,
                    &format!("/orders_daily@2?limit=1&offset={page}"),
                    None,
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{body}");
                let rows: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
                assert_eq!(rows.len(), 1, "one row per page: {body}");
                stitched.extend(rows);
            }
            assert_eq!(stitched, all, "page-by-page must equal the single read");
        });
    }

    #[test]
    fn grain_filter_narrows_and_unknown_params_are_400() {
        let router = router_with(|_| {});
        rt().block_on(async {
            let (status, body) = get(
                &router,
                "/orders_daily@2?order_date=2026-06-01&region=us-east",
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let rows: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
            assert_eq!(rows.len(), 1, "{body}");
            assert_eq!(rows[0]["region"], "us-east");

            // The exact false-confidence case the ADR names: a filter on a
            // declared-but-non-grain column must fail, not silently return
            // unfiltered rows.
            let (status, body) = get(&router, "/orders_daily@2?revenue=999", None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(body.contains("unknown query parameter 'revenue'"), "{body}");

            let (status, body) = get(&router, "/orders_daily@2?limit=abc", None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

            let (status, body) = get(
                &router,
                &format!("/orders_daily@2?offset={}", MAX_OFFSET + 1),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

            let (status, _) = get(&router, "/no_such@1", None).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        });
    }

    #[test]
    fn unshareable_cell_denies_everything_but_health() {
        let router = router_with(|cell| {
            cell.def.access.shareable = false;
        });
        rt().block_on(async {
            let (status, _) = get(&router, "/", None).await;
            assert_eq!(status, StatusCode::OK, "health stays pre-auth");
            // The document is the map — same auth tier as the data, exactly
            // authorize() (ADR 0012 §4). No lower "docs" tier.
            for uri in ["/context", "/openapi.json", "/orders_daily@2"] {
                let (status, body) = get(&router, uri, None).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
            }
        });
    }

    /// ADR 0012 §5: the open-time probe measures the real rows — coverage
    /// min/max on the date grain column, complete value lists on the string
    /// grain column, and an example_request drawn jointly from one real row.
    #[test]
    fn context_carries_probe_measurements_from_the_real_rows() {
        let router = router_with(|_| {});
        rt().block_on(async {
            let (status, body) = get(&router, "/context", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            let probe = &v["observed"]["exports"]["orders_daily@2"];
            assert_eq!(probe["rows"], 4, "{body}");
            assert_eq!(probe["coverage"]["order_date"]["min"], "2026-06-01");
            assert_eq!(probe["coverage"]["order_date"]["max"], "2026-06-02");
            assert_eq!(probe["values"]["region"]["complete"], true);
            assert_eq!(
                probe["values"]["region"]["values"],
                serde_json::json!(["eu-west", "us-east", "us-west"])
            );
            // Drawn jointly from the first row in grain order — a combination
            // that actually co-occurs. Pasting it must return exactly one row.
            let example = probe["example_request"].as_str().unwrap();
            assert_eq!(
                example,
                "/orders_daily@2?order_date=2026-06-01&region=us-east&limit=10"
            );
            let (status, rows) = get(&router, example, None).await;
            assert_eq!(status, StatusCode::OK, "{rows}");
            let rows: Vec<serde_json::Value> = serde_json::from_str(&rows).unwrap();
            assert_eq!(rows.len(), 1, "the example request must hit a real row");
        });
    }

    /// ADR 0012 §4: --no-data serves the map and withholds the rows — 404
    /// (not 403) with the engine-emitted sentence on data routes, query
    /// block and value lists omitted, coverage retained, channels surfaced.
    #[test]
    fn no_data_mode_serves_context_without_rows() {
        let router = router_mode(true, |cell| {
            cell.channels = vec!["warehouse: analytics.orders_daily".to_string()];
        });
        rt().block_on(async {
            let (status, body) = get(&router, "/orders_daily@2", None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
            assert!(
                body.contains("not served by this endpoint by design"),
                "{body}"
            );

            let (status, body) = get(&router, "/context", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["data"]["served_here"], false);
            assert_eq!(
                v["data"]["channels"],
                serde_json::json!(["warehouse: analytics.orders_daily"])
            );
            // The query block describes HTTP affordances that don't exist here.
            assert!(v["declared"]["exports"][0].get("query").is_none(), "{body}");
            let probe = &v["observed"]["exports"]["orders_daily@2"];
            // Value lists are row-derived data — withheld. Coverage stays:
            // an aggregate that names no entity.
            assert!(probe.get("values").is_none(), "{body}");
            assert!(probe.get("example_request").is_none(), "{body}");
            assert_eq!(probe["coverage"]["order_date"]["min"], "2026-06-01");
            assert_eq!(probe["rows"], 4);
            // The same sentence as the 404 body rides notes[].
            assert!(
                v["notes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|n| n.as_str() == Some(crate::context::NOTE_NO_DATA)),
                "{body}"
            );

            // OpenAPI describes the callable surface — no data paths here.
            let (status, body) = get(&router, "/openapi.json", None).await;
            assert_eq!(status, StatusCode::OK);
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(v["paths"].as_object().unwrap().is_empty(), "{body}");
        });
    }

    #[test]
    fn role_gated_cell_requires_a_known_token_with_the_role() {
        let scaffold = built_cell();
        let principals = scaffold.dir.join("principals.json");
        std::fs::write(
            &principals,
            r#"{ "good": ["analyst"], "other": ["viewer"] }"#,
        )
        .unwrap();
        let router = router_with(|cell| {
            cell.def.access.roles = vec!["analyst".to_string()];
            cell.principals = Some(principals.to_string_lossy().into_owned());
        });
        rt().block_on(async {
            let (status, _) = get(&router, "/orders_daily@2", None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "no token");
            let (status, _) = get(&router, "/orders_daily@2", Some("bogus")).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "unknown token");
            let (status, _) = get(&router, "/orders_daily@2", Some("other")).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "wrong role");
            let (status, body) = get(&router, "/orders_daily@2", Some("good")).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        });
    }

    // --- issue #6: `materialize: never` — described, not routed ------------

    #[test]
    fn never_backed_export_is_declared_with_null_query_and_absent_from_openapi() {
        let router = never_router_with(|_| {});
        rt().block_on(async {
            let (status, body) = get(&router, "/context", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            let exports = v["declared"]["exports"].as_array().unwrap();
            let stg = exports
                .iter()
                .find(|e| e["name"] == "stg")
                .expect("stg must still be declared");
            assert!(
                stg.get("query").is_some_and(|q| q.is_object()),
                "the materializing export keeps its query block: {body}"
            );
            let virtual_pii = exports
                .iter()
                .find(|e| e["name"] == "virtual_pii")
                .expect("virtual_pii must be declared even though it isn't routed (issue #6)");
            assert!(
                virtual_pii.get("query").is_none_or(|q| q.is_null()),
                "a never-backed export's query block must be null: {body}"
            );

            // No swap-time probe ran against it either — its `source_object`
            // names no lake relation.
            assert!(
                v["observed"]["exports"].get("virtual_pii@1").is_none(),
                "{body}"
            );
            let stg_probe = &v["observed"]["exports"]["stg@1"];
            assert_eq!(
                stg_probe["rows"], 2,
                "stg's own probe is unaffected: {body}"
            );

            // OpenAPI omits the never-backed path entirely (precedent:
            // --no-data hands `generate` an empty slice) but keeps the
            // materializing one.
            let (status, body) = get(&router, "/openapi.json", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            let paths = v["paths"].as_object().unwrap();
            assert!(paths.contains_key("/stg@1"), "{body}");
            assert!(!paths.contains_key("/virtual_pii@1"), "{body}");
        });
    }

    #[test]
    fn never_backed_export_data_route_404s_after_auth_naming_the_export() {
        let router = never_router_with(|_| {});
        rt().block_on(async {
            let (status, body) = get(&router, "/virtual_pii@1", None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
            assert!(body.contains("virtual_pii@1"), "{body}");
            assert!(body.contains("materialize: never"), "{body}");
            assert!(body.contains("data.channels"), "{body}");

            // The materializing sibling export still serves rows normally.
            let (status, body) = get(&router, "/stg@1", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let rows: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(rows.as_array().unwrap().len(), 2, "{body}");
        });
    }

    #[test]
    fn never_backed_export_404_is_gated_behind_auth_not_ahead_of_it() {
        // ADR 0012 §4's disclosure-boundary rule, per issue #6's review: an
        // unauthenticated caller must not be able to enumerate which
        // exports are virtual by probing routes. `authorize()` has to run
        // (and reject) BEFORE the never-backed check ever fires.
        let scaffold = built_never_cell();
        let principals = scaffold.dir.join("never_principals.json");
        std::fs::write(&principals, r#"{ "good": ["analyst"] }"#).unwrap();
        let router = never_router_with(|cell| {
            cell.def.access.roles = vec!["analyst".to_string()];
            cell.principals = Some(principals.to_string_lossy().into_owned());
        });
        rt().block_on(async {
            // No token at all: 401, never the virtual-export 404 — the
            // caller learns nothing about whether the route is real.
            let (status, body) = get(&router, "/virtual_pii@1", None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
            assert!(
                !body.contains("materialize: never"),
                "an unauthenticated caller must not learn this export is virtual: {body}"
            );

            // A correctly-authorized caller reaches the never-backed 404.
            let (status, body) = get(&router, "/virtual_pii@1", Some("good")).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
            assert!(body.contains("materialize: never"), "{body}");
        });
    }
}
