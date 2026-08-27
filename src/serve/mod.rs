mod openapi;

use anyhow::{bail, Context, Result};
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
    /// bound ones included (issue #6): the dispatch map needs to tell "no
    /// such export" (404, unknown route) apart from "this export exists but
    /// is not routed here" (404, `bound_routes`, after
    /// `authorize()`).
    routes: HashMap<String, Export>,
    /// route keys for bound exports (issue #6): not mounted, described
    /// only. Checked post-`authorize()` in
    /// `serve_export_inner` — a pre-auth check here would let an
    /// unauthenticated caller enumerate which exports are virtual.
    bound_routes: HashSet<String>,
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
    /// The interface (ADR 0015) — every claim, no measurement — precomputed:
    /// it never changes for the lifetime of the process. Warehouse column
    /// descriptions (`.cell/source_descriptions.json`) are merged in here,
    /// which is why they're loaded before it below.
    interface: crate::context::Interface,
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
    /// ADR 0012 §4).
    data_mounted: bool,
    /// `context::served_here(data_mounted, &route_list)`, computed once at
    /// construction and read from both the digest-feeding `DataBlock` (built
    /// alongside this in `build_state`) and every per-request document
    /// (`context_doc`) — never recomputed from `data_mounted` alone, which is
    /// what let an all-bound cell claim `served_here: true` while
    /// every data route 404'd (issue #17). Stable for the process lifetime:
    /// `route_list` is structural (from `cell.def`), never touched by the
    /// poller.
    served_here: bool,
    /// `config::builds_no_snapshot(&cell.transforms)`, computed once at
    /// construction. See `context::Facts::is_all_never`'s doc comment for why
    /// this is distinct from `direct_attach` and needs its own note.
    is_all_never: bool,
    /// Profile-declared locations rows actually live when not served here.
    channels: Vec<String>,
    /// Swap-time probe results (ADR 0012 §5): computed at open and at swap
    /// on the poller thread, never on the request path; omitted pieces stay
    /// omitted rather than blocking serving.
    probes: Mutex<indexmap::IndexMap<String, crate::context::ExportProbe>>,
    /// Cell-source name -> upstream ref (issue #7), precomputed once —
    /// structural, derived only from `cell.yaml`, so (like `interface`) it
    /// never changes for the life of the process. The *values*
    /// (`execution`/`data_as_of`) still ride the poller-refreshed
    /// `run_summary` cache below, read fresh on every request — this map
    /// only supplies the correlation key.
    upstream_refs: Vec<(String, String)>,
    /// Loaded docs pages (ADR 0013): content read exactly once, at startup —
    /// the mount is immutable for the life of the process, so unlike probes
    /// this needs no poller re-computation. Handlers must never touch the
    /// filesystem; this cache (and `docs_fingerprints`/`docs_etag_suffix`
    /// below) is what makes that true.
    docs_pages: Vec<crate::config::docs::DocsPage>,
    /// Docs page fingerprints (ADR 0013 §5), read from `published.json` at
    /// startup — a release-time fact, never recomputed from the live files
    /// (which would tie `docs[].sha256` to "what's on disk right now"
    /// instead of "what a release verified").
    docs_fingerprints: indexmap::IndexMap<String, crate::context::DocsFingerprint>,
    /// The docs-variant `ETag` suffix (ADR 0013 §6): a hash over `docs_pages`'
    /// sha256s in declared order, precomputed at startup — never on the
    /// request path. `"<interface_digest>~docs.<this>"` is the docs-variant
    /// ETag; the plain digest (no suffix) is the default variant's.
    docs_bundle_sha12: String,
    /// The live-verify source-check record (issue #6/#16), read once at
    /// startup via `SourceCheckRecord::fresh_for` — `None` unless a fresh
    /// (digest- and profile-matched) `.cell/source_check.json` shipped in
    /// this deploy artifact. Startup-only, not poller-refreshed: see
    /// `build_state`'s doc comment for why.
    source_check: Option<crate::context::SourceCheck>,
    /// H3: a short hash over `source_check`/`source_descriptions`,
    /// precomputed at startup exactly like `docs_bundle_sha12` — folded
    /// into every `/context` `ETag`, not just the `?include=docs` variant.
    /// Without this, `interface_digest` (declared/data only, ADR 0012 §2)
    /// is byte-identical across the rollout that first ships a passing
    /// live-verify record (`cell.yaml` is unchanged), so a caching client's
    /// `If-None-Match` would 304 straight past the entire fact this state
    /// exists to surface. `None` when neither observed input is present —
    /// the common case keeps today's byte-identical default `ETag`.
    observed_bundle_sha12: Option<String>,
    /// The URL prefix this cell is mounted at — `""` for a single cell,
    /// `"/weather"` in a project (ADR 0014). Read only by response headers
    /// and the OpenAPI `servers` block: it is deployment, never contract, so
    /// it must never reach `interface_digest` (see
    /// `context::INCLUDE_DOCS_REQUEST`).
    base_path: String,
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
    drain_timeout: u64,
) -> Result<()> {
    let cell = engine::open(file, profile, /* read_only */ true)?;
    refuse_stale_discovery(&cell)?;
    let cell_name = cell.def.cell.clone();
    // Single-cell mode mounts at the root (ADR 0014): nesting it would break
    // every deployed URL, every Kubernetes probe path, and every `mesh emit`
    // url.
    let (state, store) = build_state(cell, no_data, file, profile, /* base_path */ "")?;

    print_banner(
        port,
        &[BannerRow {
            mount: "/".to_string(),
            cell: cell_name,
            profile: profile.to_string(),
            no_data,
        }],
        /* project */ false,
    );

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
            engine::Budget::default(),
            /* stagger */ 0,
        );
    }

    let app = app(state, max_concurrency);
    bind_and_serve(app, port, drain_timeout).await
}

/// Serve N cells from one process, each mounted at its own prefix
/// (ADR 0014). Every cell keeps its own connection, catalog, poller,
/// principals file, authorization policy, and concurrency cap — the process
/// is shared, nothing else is. Nothing here fetches a cell over HTTP or
/// aggregates across cells: that would be the server-side aggregator ADR
/// 0012 §6 refuses by name.
/// A project's mounted cells: mount segment (no leading `/`) paired with its
/// serving state.
type MountedCells = Vec<(String, Arc<AppState>)>;

pub async fn run_project(
    project: &crate::project::Project,
    port: u16,
    poll_interval: u64,
    max_concurrency: usize,
    no_data: bool,
    drain_timeout: u64,
) -> Result<()> {
    let n = project.cells.len();
    let budget = project_budget(n)?;

    // Cells open **sequentially**, never concurrently: every `setup` runs
    // `INSTALL ducklake; INSTALL json` against the shared
    // `~/.duckdb/extensions` directory, and N simultaneous installs race on
    // the same files.
    let mut mounted: MountedCells = Vec::new();
    let mut rows: Vec<BannerRow> = Vec::new();
    let mut pollers = Vec::new();
    for (nth, pc) in project.cells.iter().enumerate() {
        // Every listed cell must open or the server does not start. The
        // precedent is `load_principals` two functions up: a swallowed
        // startup error yields a process that looks healthy on `/` while
        // serving a 404 no caller can tell from a typo. An operator who wants
        // a cell out removes it from the project file — an explicit,
        // reviewable act.
        let cell = engine::open_with(&pc.file, &pc.profile, /* read_only */ true, &budget)
            .and_then(|c| refuse_stale_discovery(&c).map(|_| c))
            .with_context(|| {
                format!(
                    "opening cells[{}] ({}, profile `{}`) declared in {} — every listed cell \
                     must open or the server does not start; fix the profile or remove the entry",
                    pc.index,
                    pc.file.display(),
                    pc.profile,
                    project.file.display()
                )
            })?;

        let mount = match &pc.mount {
            Some(m) => m.clone(),
            None => {
                crate::project::check_derived_mount(&cell.def.cell, &pc.file, pc.index)?;
                cell.def.cell.clone()
            }
        };
        let base_path = format!("/{mount}");
        let cell_no_data = no_data || pc.no_data;
        let cell_name = cell.def.cell.clone();

        let (state, store) = build_state(cell, cell_no_data, &pc.file, &pc.profile, &base_path)?;
        if let Some(store) = &store {
            fetch_run_summary(&state, store);
        }
        if let Some(store) = store {
            // Stagger the pollers: N threads sleeping on one interval wake
            // together and issue N simultaneous artifact downloads forever.
            pollers.push((state.clone(), store, pc.clone(), nth as u64));
        }

        rows.push(BannerRow {
            mount: base_path.clone(),
            cell: cell_name,
            profile: pc.profile.clone(),
            no_data: cell_no_data,
        });
        mounted.push((mount, state));
    }

    let mount_index: Vec<(usize, String)> = project
        .cells
        .iter()
        .zip(&mounted)
        .map(|(pc, (mount, _))| (pc.index, mount.clone()))
        .collect();
    crate::project::check_unique_mounts(&mount_index)?;

    for (state, store, pc, nth) in pollers {
        spawn_poller(
            state,
            store,
            pc.file.clone(),
            pc.profile.clone(),
            poll_interval.max(1),
            budget.clone(),
            nth,
        );
    }

    print_banner(port, &rows, /* project */ true);

    bind_and_serve(
        project_router(mounted, max_concurrency),
        port,
        drain_timeout,
    )
    .await
}

/// Assemble the project's router: `/` (outside every cell's throttle, so no
/// cell's saturation can shed the process's own liveness route), each cell
/// nested at its mount with its own throttle, and a fallback that names the
/// door that does exist. Split from `run_project` so it can be driven with
/// `oneshot` in tests, same idiom as `build_state`/`app`.
fn project_router(mounted: MountedCells, max_concurrency: usize) -> Router {
    let root = Router::new()
        .route("/", get(project_root))
        .with_state(Arc::new(mounted.clone()));
    let mut app = root;
    for (mount, state) in mounted {
        // `nest_service`, not `nest`: axum 0.7's `nest` maps the nested `/`
        // route to the bare prefix only (`/weather`, never `/weather/`) —
        // `nest_service` registers both, which is the one every operator
        // typing a mount URL by hand should be able to rely on.
        app = app.nest_service(
            &format!("/{mount}"),
            throttle(cell_router(state), max_concurrency),
        );
    }
    // A wrong mount is a 404 that names the door that does exist, matching
    // `no export '{route}'` on the data door.
    app.fallback(project_not_found)
}

/// Serve until SIGTERM/SIGINT, then drain: the listener stops accepting
/// (a readiness probe on `/` fails from this moment, so an orchestrator
/// routes new traffic elsewhere), in-flight requests get up to
/// `drain_timeout` seconds to finish, and the process exits 0. A
/// handler-less PID 1 would ignore SIGTERM and sit through the whole
/// termination grace period before being SIGKILLed mid-request; the
/// default disposition would drop every in-flight request instantly.
async fn bind_and_serve(app: Router, port: u16, drain_timeout: u64) -> Result<()> {
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = std::future::IntoFuture::into_future(
        axum::serve(listener, app).with_graceful_shutdown(async move {
            let _ = stop_rx.await;
        }),
    );
    tokio::pin!(server);
    tokio::select! {
        // The server ended on its own (an accept error) — surface it.
        res = &mut server => {
            res?;
            Ok(())
        }
        signal = shutdown_signal() => {
            tracing::info!(
                signal,
                drain_timeout_seconds = drain_timeout,
                "shutdown requested: no longer accepting connections; draining in-flight requests"
            );
            let _ = stop_tx.send(());
            match tokio::time::timeout(std::time::Duration::from_secs(drain_timeout), &mut server)
                .await
            {
                Ok(res) => {
                    res?;
                    tracing::info!("stopped: in-flight requests drained");
                }
                Err(_) => tracing::warn!(
                    drain_timeout_seconds = drain_timeout,
                    "stopped: drain timeout exceeded with requests still in flight"
                ),
            }
            Ok(())
        }
    }
}

/// Resolves on SIGTERM or SIGINT (Ctrl-C), naming which.
async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing the SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = term.recv() => "SIGTERM",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "SIGINT"
    }
}

/// Divide the operator's DuckDB memory budget across the mounted cells
/// (ADR 0014). `DATAMK_MEMORY_LIMIT` is applied per connection, so N cells
/// would otherwise authorize N times what the operator wrote — and the pod
/// OOM-kills, taking every cell with it. An unparseable value is a hard
/// startup error rather than a warning: silently handing each of ten cells
/// the full budget is the exact failure the knob exists to prevent.
fn project_budget(cells: usize) -> Result<engine::Budget> {
    let cells = cells.max(1);
    let memory_limit = match std::env::var("DATAMK_MEMORY_LIMIT") {
        Ok(raw) if !raw.is_empty() => {
            let bytes = parse_bytes(&raw).with_context(|| {
                format!(
                    "DATAMK_MEMORY_LIMIT='{raw}' cannot be divided across {cells} mounted cells \
                     — use a value like 3GB, or unset it and let DuckDB size itself"
                )
            })?;
            let each = (bytes / cells as u64).max(1 << 20);
            Some(format!("{}MiB", (each / (1 << 20)).max(1)))
        }
        _ => None,
    };
    // DuckDB defaults `threads` to the host's core count per connection.
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    Ok(engine::Budget {
        memory_limit,
        threads: Some((cores / cells).max(1)),
    })
}

/// Parse a DuckDB-style byte size (`3GB`, `512MiB`, `1024`) into bytes.
/// Decimal and binary suffixes both accepted, both treated as binary — the
/// 7% difference is noise against a budget being divided N ways, and
/// over-counting is the unsafe direction.
fn parse_bytes(raw: &str) -> Result<u64> {
    let s = raw.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num
        .parse()
        .with_context(|| format!("'{raw}' does not start with a number"))?;
    let mult: u64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        "T" | "TB" | "TIB" => 1u64 << 40,
        other => bail!("unknown size unit '{other}'"),
    };
    Ok((num * mult as f64) as u64)
}

/// One row of the startup banner.
struct BannerRow {
    mount: String,
    cell: String,
    profile: String,
    no_data: bool,
}

/// What is mounted where, at which profile — printed before the socket
/// binds. A per-cell `profile:` override needs to be visible at startup,
/// not just written down in the project file.
fn print_banner(port: u16, rows: &[BannerRow], project: bool) {
    let plural = if rows.len() == 1 { "cell" } else { "cells" };
    println!(
        "\nServing {} {plural} on http://0.0.0.0:{port}\n",
        rows.len()
    );
    let w_mount = rows.iter().map(|r| r.mount.len()).max().unwrap_or(5).max(5);
    let w_cell = rows.iter().map(|r| r.cell.len()).max().unwrap_or(4).max(4);
    let w_prof = rows
        .iter()
        .map(|r| r.profile.len())
        .max()
        .unwrap_or(7)
        .max(7);
    println!(
        "  {:<w_mount$}  {:<w_cell$}  {:<w_prof$}  DATA",
        "MOUNT", "CELL", "PROFILE"
    );
    for r in rows {
        let data = if r.no_data { "no (no_data)" } else { "yes" };
        println!(
            "  {:<w_mount$}  {:<w_cell$}  {:<w_prof$}  {data}",
            r.mount, r.cell, r.profile
        );
    }
    println!();
    if project {
        let first = rows.first().map(|r| r.mount.as_str()).unwrap_or("/cell");
        let root_line = "  GET /".to_string();
        let context_line = format!("  GET {first}/context");
        let w = root_line.len().max(context_line.len()) + 3;
        println!("{root_line:<w$}the mounted cells");
        println!("{context_line:<w$}a cell's interface");
    } else {
        println!("  GET /                  liveness");
        println!("  GET /context           the cell's interface");
    }
    println!();
}

/// ADR 0016 §5: a discovered cell with no fresh sidecar record must not
/// start — it would serve a valid ETag over an empty interface,
/// indistinguishable from "this cell has no exports" (ADR 0014 §6, startup
/// is strict). Named, never silent.
fn refuse_stale_discovery(cell: &engine::Cell) -> Result<()> {
    if let Some(crate::config::Discovery::Stale(why)) = &cell.discovery {
        anyhow::bail!(
            "cell '{}' discovers its interface, but {why}. `serve` refuses to start over an \
             empty interface.",
            cell.def.cell
        );
    }
    Ok(())
}

/// Build the serving state from an opened cell. Split from `run` so the
/// in-process smoke tests can stand up the exact router `serve` binds,
/// without a socket. Returns the store handle separately (published mode)
/// so `run` can hand it to the poller.
///
/// `file`/`profile` are `run`'s own params, threaded through (not read from
/// `cell`, which carries none of this): issue #16 needs them to compute
/// `cell_yaml_digest_of(file)` and gate `.cell/source_check.json` on
/// `profile`, both at startup only — matching the customer's own read that
/// a Server's `/context` should mean "verified against the exact bytes this
/// pod ships with," not drift independently between rollouts.
fn build_state(
    cell: engine::Cell,
    no_data: bool,
    file: &Path,
    profile: &str,
    base_path: &str,
) -> Result<(Arc<AppState>, Option<Arc<crate::store::Store>>)> {
    let published = load_published(&cell.dir);
    let data_mounted = !no_data;

    // The one visibility-filtered route list (ADR 0012 §4): the router's
    // dispatch map, the OpenAPI doc, and the context document all derive
    // from this single call — never three independent predicates. Includes
    // bound exports (issue #6, binding model) — the interface is unconditional
    // (datamk owns the contract regardless of who owns the rows); `mounted`
    // is the snapshot-backed subset actually routed over HTTP.
    let all_routes = crate::context::discoverable_routes(&cell.def)?;
    let is_all_never = crate::config::builds_no_snapshot(&cell.transforms);
    let mounted = crate::context::mounted_routes(&all_routes);
    let bound_routes: HashSet<String> = all_routes
        .iter()
        .filter(|(_, e)| e.is_bound())
        .map(|(route, _)| route.clone())
        .collect();
    let routes: HashMap<String, Export> = all_routes.iter().cloned().collect();
    // The query block is unconditional interface grammar (ADR 0012 §2,
    // amended) — never gated on --no-data. It's still null per-export for a
    // bound export, regardless of --no-data: that's a genuine interface
    // fact (issue #6), not a serving-mode one.
    // Issue #6/#10: the live-verify source-descriptions record
    // (`.cell/source_descriptions.json`), read once at startup via
    // `SourceDescriptionsRecord::fresh_for` — it lands on the bound exports'
    // columns inside the interface (ADR 0015 §4), so it's loaded first.
    // Startup-only, not poller-refreshed: it ships inside the same deploy
    // artifact as `cell.yaml`, so a new record can only reach a running pod
    // through a rollout (see `source_check` below for the full argument).
    let cell_yaml_digest = crate::context::cell_yaml_digest_of(file)?;
    let source_descriptions: indexmap::IndexMap<String, indexmap::IndexMap<String, String>> =
        crate::manifest::SourceDescriptionsRecord::fresh_for(&cell.dir, &cell_yaml_digest, profile)
            .map(|r| r.sources.into_iter().collect())
            .unwrap_or_default();
    let interface = crate::context::interface(&cell.def, &all_routes, &source_descriptions);
    // Computed once, stored, and read from both the digest (below, via
    // `data`) and the per-request document (`context_doc`'s `s.served_here`)
    // — one fact, not two calls that could independently drift, since the
    // document must never disagree with its own ETag.
    let served_here = crate::context::served_here(data_mounted, &mounted);
    let data = crate::context::DataBlock {
        served_here,
        channels: cell.channels.clone(),
    };
    let digest = crate::context::interface_digest(&cell.def.cell, &interface, &data);
    // Issue #7: structural only (source name -> upstream ref) — the
    // correlation key `context_doc` pairs against the poller-refreshed
    // `run_summary` cache on every request, never precomputed values.
    let upstream_refs = crate::context::cell_source_refs(&cell.def);

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
    let direct_attach = cell.direct_attach;
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

    // ADR 0013: docs pages read exactly once, at startup — content for the
    // `?include=docs` door and the bundle sha the docs-variant ETag hashes
    // over. An unreadable/oversized/empty/non-UTF-8 page fails `serve` at
    // startup (matching `load_principals`'s discipline), not on the first
    // request that happens to ask for it.
    let docs_pages = crate::config::docs::load_declared(&cell.dir, &cell.def, &all_routes)?;
    let docs_bundle_sha12 = docs_bundle_sha12(&docs_pages);
    // Fingerprints are a release-time fact (ADR 0013 §5) — read from
    // `published.json`, never recomputed from the live files.
    let docs_fingerprints: indexmap::IndexMap<String, crate::context::DocsFingerprint> =
        crate::manifest::Published::load(&cell.dir)
            .map(|p| p.docs.into_iter().collect())
            .unwrap_or_default();

    // Issue #16: the live-verify source-check record, read once at startup
    // — matches `docs_pages`/`docs_fingerprints` above, not the poller-
    // refreshed `run_summary` cache. Startup-only is deliberate, not a
    // shortcut: the record ships inside the same deploy artifact as
    // `cell.yaml` (`.cell/source_check.json`, folded into `content_hash`),
    // so a new verify-check can only ever reach a running pod through a
    // rollout — polling for it independently would just be a slower way to
    // notice the same rollout the checksum annotation already rolls on.
    // `fresh_for` is the one place the digest+profile match happens
    // (`context::build_document` calls the same function).
    let source_check =
        crate::manifest::SourceCheckRecord::fresh_for(&cell.dir, &cell_yaml_digest, profile)
            .as_ref()
            .map(|r| crate::context::SourceCheck::from_record(r, &all_routes));
    // H3: precomputed once here, from the exact two values above — never
    // recomputed on the request path, same discipline as `docs_bundle_sha12`.
    let observed_bundle_sha12 = observed_bundle_sha12(source_check.as_ref(), &source_descriptions);

    let state = Arc::new(AppState {
        // Under --no-data the OpenAPI paths are empty: the spec describes
        // the callable HTTP surface, and the data routes are not mounted.
        // Never-backed exports are always excluded (issue #6) — `mounted`
        // already dropped them regardless of --no-data.
        openapi: openapi::generate_with_all(
            &cell.def.cell,
            cell.def.description.as_deref(),
            if data_mounted { &mounted } else { &[] },
            &all_routes,
            &digest,
            base_path,
        ),
        cell_name: cell.def.cell.clone(),
        routes,
        bound_routes,
        published,
        shareable: cell.def.access.shareable,
        allowed_roles: cell.def.access.roles.clone(),
        principals,
        execution: std::sync::atomic::AtomicU64::new(execution),
        freshness: Mutex::new(Freshness {
            latest_seen: execution,
            last_ok_poll_unix: store.as_ref().map(|_| unix_now()),
        }),
        interface,
        digest,
        direct_attach,
        run_summary: Mutex::new(None),
        data_as_of: Mutex::new(data_as_of),
        route_list: mounted,
        data_mounted,
        served_here,
        is_all_never,
        channels: cell.channels.clone(),
        probes: Mutex::new(probes),
        upstream_refs,
        docs_pages,
        docs_fingerprints,
        docs_bundle_sha12,
        source_check,
        observed_bundle_sha12,
        base_path: base_path.to_string(),
        cell: Mutex::new(cell),
    });
    Ok((state, store))
}

/// The docs-variant `ETag` suffix (ADR 0013 §6): a hash over every page's
/// sha256 in declared order, truncated to 12 hex chars — same convention as
/// the Kubernetes ConfigMap's content-hash truncation
/// (`deploy/targets/kubernetes/render.rs`'s `content_hash_short`). Order-
/// and identity-sensitive (a page rename changes `target`, which isn't fed
/// in here — that's covered by `interface_digest` instead; this suffix
/// tracks *content* only).
fn docs_bundle_sha12(pages: &[crate::config::docs::DocsPage]) -> String {
    let joined = pages
        .iter()
        .map(|p| p.sha256.as_str())
        .collect::<Vec<_>>()
        .join(",");
    crate::context::sha256_hex(joined.as_bytes())[..12].to_string()
}

/// The `observed`-variant `ETag` suffix (issue #16 H3): a short hash over
/// the two observed inputs that are fixed at startup but arrive via a
/// rollout with `cell.yaml` byte-identical — `source_check.checked_at` and
/// `source_descriptions` — same idiom as `docs_bundle_sha12` (truncated
/// sha256, precomputed at startup, never on the request path). `None` when
/// neither observed input is present at all, so a cell with no live-verify
/// record keeps the exact byte-identical default `ETag` it always had —
/// this only ever adds a suffix where there is something new to invalidate
/// a cache over.
fn observed_bundle_sha12(
    source_check: Option<&crate::context::SourceCheck>,
    source_descriptions: &indexmap::IndexMap<String, indexmap::IndexMap<String, String>>,
) -> Option<String> {
    if source_check.is_none() && source_descriptions.is_empty() {
        return None;
    }
    let checked_at = source_check.map(|sc| sc.checked_at.as_str()).unwrap_or("");
    // The measurements ride the same variant: two checks a second apart with
    // different counts must not share an ETag.
    let measurements_json = source_check
        .map(|sc| serde_json::to_string(&sc.exports).unwrap_or_default())
        .unwrap_or_default();
    // `source_descriptions` is deterministic within one process's lifetime
    // (built once, at startup, from a `BTreeMap`-sorted record) — good
    // enough for a cache-invalidation hash, which only needs to change when
    // the content genuinely does, not to be canonical across processes.
    let descriptions_json = serde_json::to_string(source_descriptions).unwrap_or_default();
    let joined = format!("{checked_at}|{measurements_json}|{descriptions_json}");
    Some(crate::context::sha256_hex(joined.as_bytes())[..12].to_string())
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
    // One timestamp for the whole pass — the measurement's `at` (ADR 0015 §3).
    let probed_at = crate::timeutil::rfc3339_utc(crate::timeutil::unix_now());
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
        let mut probe = ExportProbe::at(probed_at.clone());
        probe.rows = conn
            .prepare(&format!("SELECT count(*) FROM {source}{at}"))
            .and_then(|mut s| s.query_row([], |r| r.get::<_, i64>(0)))
            .ok();

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
                    // Relative to the document's own URL, exactly like
                    // `sample_request` — this one is a measurement (`probe`) and never
                    // reaches the digest, but a caller must resolve both the
                    // same way.
                    probe.example_request = Some(format!("{route}?{params}&limit=10"));
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
    throttle(cell_router(state), max_concurrency)
}

/// One cell's routes, unthrottled. Split from `app` so a project can give
/// each mounted cell its **own** throttle: a tower layer instance owns one
/// semaphore, so a shared layer would let one cell's saturation shed every
/// other cell's requests — including the liveness route Kubernetes probes
/// (`deploy/targets/kubernetes/render.rs`'s `http_probe`), turning one slow
/// query into an estate-wide pod restart.
fn cell_router(state: Arc<AppState>) -> Router {
    // `/context` replaces the old `/interface` stub (ADR 0012 §4): renamed,
    // not duplicated — one document, one route, and the old name 404s (same
    // rule as unmounted data routes: no door, no 403). No reserved-name
    // collision: export routes always carry the major (`name@major`), so an
    // export named `context` serves at `/context@1`.
    Router::new()
        .route("/", get(health))
        .route("/context", get(context_doc))
        // One export's slice of the same document (ADR 0012 §4, amended
        // 2026-08-27): an agent answering one question fetches one contract
        // and one page. Same shape, own ETag variant.
        .route("/context/:route", get(context_export))
        .route("/openapi.json", get(openapi_doc))
        .route("/:route", get(serve_export))
        .with_state(state)
}

/// The throttle stack (ADR 0012 §7): a concurrency cap with load-shed.
/// Requests over the cap get an immediate 503 instead of queueing without
/// bound. Applied per mounted cell (see `cell_router`), so `--max-concurrency`
/// keeps its single-cell meaning exactly. Per-client fairness is a reverse
/// proxy's job (docs/guides/serving.md), not this socket's.
fn throttle(router: Router, max_concurrency: usize) -> Router {
    router.layer(
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
    budget: engine::Budget,
    stagger: u64,
) {
    // Every line a poller emits carries `cell` (ADR 0014): N pollers
    // interleaved on one process's stderr are otherwise unattributable, and
    // "failed to open newly published execution" is precisely the line an
    // operator needs to trace to a cell.
    let cell_name = state.cell_name.clone();
    std::thread::spawn(move || {
        // Offset each cell's first tick so N pollers don't wake together and
        // issue N simultaneous artifact downloads on every interval.
        if stagger > 0 {
            let offset = (stagger % interval_secs.max(1)).min(interval_secs);
            std::thread::sleep(std::time::Duration::from_secs(offset));
        }
        loop {
            std::thread::sleep(std::time::Duration::from_secs(interval_secs));

            let latest = match store.latest() {
                Ok(Some(n)) => n,
                Ok(None) => {
                    tracing::warn!(cell = %cell_name, "LATEST pointer disappeared; keeping last-good catalog");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(cell = %cell_name, error = %e, "polling LATEST failed; keeping last-good catalog");
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
                match engine::open_with(&file, &profile, /* read_only */ true, &budget) {
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
                        tracing::info!(cell = %cell_name, execution = n, "swapped to newly published execution");
                    }
                    Err(e) => {
                        tracing::warn!(cell = %cell_name, error = %e, execution = latest,
                        "failed to open newly published execution; keeping last-good catalog");
                    }
                }
            }

            // ADR 0012 §5: the run-summary fetch rides every poll tick, never the
            // swap branch — the summary is written after `publish_execution`
            // returns, so gating it on the swap would orphan any summary that
            // lands after `LATEST` advances.
            fetch_run_summary(&state, &store);
        }
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

/// The project root (ADR 0014): liveness, plus the mounts this caller may
/// actually reach.
///
/// The listing is filtered through each cell's **own** `authorize()`, which
/// is what keeps it on the right side of ADR 0012 §6's refusal of "a
/// data-plane route that enumerates other cells… pre-auth crawl bait": an
/// anonymous caller against an all-private project gets `[]`, and a cell
/// whose `access.shareable` is false is invisible to everyone, exactly as it
/// is today. It carries names only — no exports, no descriptions, no
/// digests. The moment it returns a per-cell summary it *is* a served mesh
/// manifest, which §6 refuses by name; that document is `datamk mesh emit`'s
/// job and it is a static file an operator hosts, never this socket.
///
/// `status` stays unconditional so a liveness probe needs no credential.
async fn project_root(
    State(cells): State<Arc<MountedCells>>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let visible: Vec<&str> = cells
        .iter()
        .filter(|(_, s)| authorize(s, &headers).is_ok())
        .map(|(mount, _)| mount.as_str())
        .collect();
    Json(serde_json::json!({ "status": "ok", "cells": visible }))
}

/// An unknown mount in a project. Names the door that exists rather than
/// listing the ones that do — an unauthenticated 404 must not become the
/// enumeration `project_root` just took care to filter.
async fn project_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        "no such cell or route — each cell mounts its own interface at \
         /<cell>/context; GET / lists the cells you can reach",
    )
        .into_response()
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

/// The closed `include=` vocabulary (ADR 0013 §4) — the single source both
/// `validate_include` and `openapi::generate`'s documented `enum` read, so
/// the two can never drift.
pub(crate) const INCLUDE_SECTIONS: &[&str] = &["docs"];

/// Validate `/context`'s query string against the closed grammar (ADR 0013
/// §4): only `include` is accepted, and its value is a comma-separated list
/// drawn from `INCLUDE_SECTIONS`. Returns the requested sections
/// (deduplicated, insertion order) or the exact 400 message. Takes the raw
/// pairs (not a `HashMap`) so repeated keys are seen rather than silently
/// collapsed — irrelevant for the one key this grammar has today, but the
/// discipline `serve_export`'s `validate_params` already established.
fn validate_include(pairs: &[(String, String)]) -> std::result::Result<Vec<String>, String> {
    let mut sections: Vec<String> = Vec::new();
    for (k, v) in pairs {
        if k != "include" {
            return Err(format!(
                "unknown query parameter '{k}' — `/context` accepts `include` (sections: docs)"
            ));
        }
        for tok in v.split(',') {
            if tok.is_empty() {
                // Covers both an entirely empty value (`?include=`, whose
                // single `split` item is `""`) and a trailing/leading/double
                // comma (`?include=docs,`) with one check.
                return Err(
                    "`include` must name at least one section — `/context` accepts: docs"
                        .to_string(),
                );
            }
            if !INCLUDE_SECTIONS.contains(&tok) {
                return Err(format!(
                    "unknown `include` section '{tok}' — `/context` accepts: docs"
                ));
            }
            if !sections.iter().any(|s| s == tok) {
                sections.push(tok.to_string());
            }
        }
    }
    Ok(sections)
}

/// `GET /context` (ADR 0012, docs door: ADR 0013 §4). Same auth tier as the
/// data — the document is the map (grain, columns, upstream refs); no lower
/// "docs" tier, no pre-auth serving. Handlers touch no store and no DuckDB:
/// everything here reads precomputed state and the poller-maintained caches
/// — docs content and fingerprints included, both loaded once at startup.
///
/// `?include=docs` inlines docs page content and switches the `ETag` to the
/// docs variant (`"<digest>~docs.<bundle sha>"`); the plain `GET /context`
/// keeps the byte-identical default `ETag` it always had. Any other query
/// param, or an unrecognized/empty `include` section, is a 400 — silently
/// ignoring `?include=dcos` would return `content: null`, read by an agent
/// as "no docs", the exact false-confidence failure `validate_params`
/// already exists to kill on the data door.
async fn context_doc(
    State(s): State<Arc<AppState>>,
    Query(pairs): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = authorize(&s, &headers) {
        return resp;
    }

    let sections = match validate_include(&pairs) {
        Ok(s) => s,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    let want_docs = sections.iter().any(|s| s == "docs");

    // The interface digest names the interface; the ETag names a
    // representation of it (ADR 0013 §6) — both suffixes are precomputed at
    // startup, never on the request path, and layer independently:
    // `~observed.<hash>` (H3) folds in `source_check`/`source_descriptions`
    // — startup-fixed facts that can newly appear across a rollout with
    // `cell.yaml` byte-identical, which `interface_digest` alone would miss
    // entirely (declared/data only, ADR 0012 §2) — and is present on every
    // variant, default included, whenever there's an observed input to
    // cover; `~docs.<bundle sha>` (ADR 0013 §6) adds the docs-content
    // bundle only for `?include=docs`. A cell with neither observed input
    // present keeps the exact byte-identical default ETag it always had
    // (mesh.rs copies this verbatim into `context_digest`).
    let etag = context_etag(&s, want_docs, None);
    if matches_etag(&headers, &etag) {
        return not_modified(etag);
    }
    let mut doc = build_context_document(&s);
    if want_docs {
        doc.inline_docs(&s.docs_pages);
    }
    context_response(etag, doc)
}

/// `GET /context/{route}`: the document narrowed to one export. 404 —
/// post-auth, like a data route — names the routes that exist, so a wrong
/// guess is one hop from a right one.
async fn context_export(
    State(s): State<Arc<AppState>>,
    axum::extract::Path(route): axum::extract::Path<String>,
    Query(pairs): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = authorize(&s, &headers) {
        return resp;
    }
    let sections = match validate_include(&pairs) {
        Ok(s) => s,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    let want_docs = sections.iter().any(|s| s == "docs");
    if !s.routes.contains_key(&route) {
        let mut known: Vec<&str> = s.routes.keys().map(String::as_str).collect();
        known.sort_unstable();
        return (
            StatusCode::NOT_FOUND,
            format!(
                "no export '{route}' — discoverable exports: {}. See GET {}/context.",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                },
                s.base_path
            ),
        )
            .into_response();
    }
    let etag = context_etag(&s, want_docs, Some(&route));
    if matches_etag(&headers, &etag) {
        return not_modified(etag);
    }
    let mut doc = build_context_document(&s);
    doc.narrow_to(&route);
    if want_docs {
        let pages: Vec<&crate::config::docs::DocsPage> = s
            .docs_pages
            .iter()
            .filter(|p| p.target == "cell" || p.target == route)
            .collect();
        doc.inline_docs(pages);
    }
    context_response(etag, doc)
}

/// The ETag for one representation of the context document (ADR 0013 §6).
/// The interface digest names the interface; the ETag names a
/// representation — the suffixes are precomputed at startup, never on the
/// request path, and layer independently: `~observed.<hash>` (H3) folds in
/// `source_check`/`source_descriptions` — startup-fixed facts that can newly
/// appear across a rollout with `cell.yaml` byte-identical — and is present
/// on every variant whenever there's an observed input; `~export.<route>`
/// names a per-export narrowing; `~docs.<bundle sha>` adds the docs-content
/// bundle for `?include=docs`. A cell with neither observed input present
/// keeps the exact byte-identical default ETag it always had (mesh.rs copies
/// this verbatim into `context_digest`).
fn context_etag(s: &AppState, want_docs: bool, export: Option<&str>) -> String {
    let mut etag = format!("\"{}", s.digest);
    if let Some(obs) = &s.observed_bundle_sha12 {
        etag.push_str(&format!("~observed.{obs}"));
    }
    if let Some(route) = export {
        etag.push_str(&format!("~export.{route}"));
    }
    if want_docs {
        etag.push_str(&format!("~docs.{}", s.docs_bundle_sha12));
    }
    etag.push('"');
    etag
}

fn matches_etag(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag)
}

fn not_modified(etag: String) -> Response {
    (
        StatusCode::NOT_MODIFIED,
        [
            (header::ETAG, etag),
            (header::CACHE_CONTROL, "private".to_string()),
        ],
    )
        .into_response()
}

fn context_response(etag: String, doc: crate::context::ContextDocument) -> Response {
    (
        StatusCode::OK,
        [
            (header::ETAG, etag),
            (header::CACHE_CONTROL, "private".to_string()),
        ],
        Json(doc),
    )
        .into_response()
}

/// The live document, assembled from the request-time facts (the poller's
/// caches) on top of the startup-fixed interface. Shared by the whole-cell
/// and per-export doors so the two can never disagree.
fn build_context_document(s: &AppState) -> crate::context::ContextDocument {
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

    // Issue #7: `upstream_refs` is precomputed structure (never changes);
    // the values ride the same poller-refreshed `run_summary` cache as
    // `provenance`, gated by the same execution-match guard above.
    let upstreams = s
        .run_summary
        .lock()
        .expect("run_summary mutex poisoned")
        .as_ref()
        .filter(|summary| summary.execution == execution)
        .map(|summary| crate::context::observed_upstreams_from(&s.upstream_refs, summary))
        .unwrap_or_default();

    let probes = s.probes.lock().expect("probes mutex poisoned").clone();
    let mut doc = crate::context::assemble(crate::context::Facts {
        cell: s.cell_name.clone(),
        interface: s.interface.clone(),
        provenance,
        // Issue #16: the record `datamk verify` persisted, read once at
        // startup (`build_state`) — not a live warehouse check performed by
        // `serve` itself, which stays credential-light exactly as issue #6
        // Q1 decided. This closes the gap where an all-`never` cell's
        // hosted `/context` was permanently `draft` even though `datamk
        // context` reported `verified_at_source` from the same record, at
        // the same moment, in the same directory.
        source_check: s.source_check.clone(),
        freshness,
        upstreams,
        probes,
        docs_fingerprints: s.docs_fingerprints.clone(),
        served_here: s.served_here,
        channels: s.channels.clone(),
        direct_attach: s.direct_attach,
        is_all_never: s.is_all_never,
    });
    if !s.data_mounted {
        // The same engine-emitted sentence the unmounted routes' 404 body
        // carries (ADR 0012 §4).
        doc.notes.push(crate::context::NOTE_NO_DATA.to_string());
    } else if !s.served_here && !s.routes.is_empty() {
        // issue #17: `data_mounted` is true (no `--no-data`) but nothing is
        // mounted anyway. `mounted` (issue #6) is `s.routes` filtered to
        // non-bound exports, so `mounted.is_empty()` while `s.routes` is
        // non-empty can only mean every discoverable export is bound —
        // that inference only actually holds when `s.routes` itself is
        // non-empty (M2): a cell with zero *discoverable* exports at all
        // (every one `visibility: private`, materializing or not) also
        // reaches `!served_here` with nothing bound anywhere, and the note
        // below would be a false claim about that cell's exports. Distinct
        // from `NOTE_NO_DATA`: this names the cell's own definition as the
        // reason, not an operator flag the reader could change with a
        // deploy argument.
        doc.notes
            .push(crate::context::NOTE_NO_ROUTES_MOUNTED.to_string());
    }
    doc
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
    // Mount-aware (ADR 0014): a hardcoded `</context>` points at the
    // *process* root, which in a project is either another cell's document or
    // a 404 — on the exact responses (400/404) where the back-link is the
    // whole point.
    if let Ok(v) = HeaderValue::from_str(&format!("<{}/context>; rel=\"describedby\"", s.base_path))
    {
        h.insert(header::LINK, v);
    }
    // One origin serves many cells with different auth policies, so a shared
    // cache keyed on URI alone could hand cell A's rows to a caller holding
    // only cell B's token (`context_doc`/`openapi_doc` say `private` too).
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private"));
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
    // `authorize()` is all-or-nothing; a shared cache keyed on URI alone
    // could otherwise hand a cached 200 to a tokenless caller (ADR 0013 §6).
    (
        [(header::CACHE_CONTROL, "private")],
        Json(s.openapi.clone()),
    )
        .into_response()
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
    // it's bound, not materialized — no lake table backs it, so there is no
    // data route to serve. Runs post-`authorize()` (the caller
    // of this function already checked): a pre-auth 404 here, unlike the
    // cell-wide --no-data check above, would let an unauthenticated caller
    // enumerate which exports are virtual one route at a time.
    if s.bound_routes.contains(&route) {
        return (
            StatusCode::NOT_FOUND,
            crate::context::note_bound_export(&route),
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
            bind: None,
            description: None,
            docs: None,
            grain: vec!["order_date".to_string(), "region".to_string()],
            schema,
            freshness: None,
            visibility: Visibility::Discoverable,
            contract: Contract::Experimental,
            from: Default::default(),
            discovered: None,
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
        assert_eq!(path, "orders_daily@2");
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

    /// The export record for `route` in a v4 document (ADR 0015).
    fn export_doc<'a>(v: &'a serde_json::Value, route: &str) -> &'a serde_json::Value {
        v["exports"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["route"] == route)
            .unwrap_or_else(|| panic!("no export {route} in {v}"))
    }

    /// The docs record for `target` in a v4 document (ADR 0015).
    fn docs_page<'a>(v: &'a serde_json::Value, target: &str) -> &'a serde_json::Value {
        v["docs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["target"] == target)
            .unwrap_or_else(|| panic!("no docs page {target} in {v}"))
    }
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
                from: None,
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
        let cell_yaml = scaffold.dir.join("cell.yaml");
        let mut cell = engine::open(&cell_yaml, "local", true).expect("open built cell read-only");
        mutate(&mut cell);
        let (state, _store) =
            build_state(cell, no_data, &cell_yaml, "local", "").expect("build state");
        app(state, DEFAULT_MAX_CONCURRENCY)
    }

    fn router_with(mutate: impl FnOnce(&mut engine::Cell)) -> Router {
        router_mode(false, mutate)
    }

    /// A MIXED cell (issue #6): one materializing export (`stg`) and one
    /// bound export (`virtual_pii`) — the shape "described, not routed" is
    /// built to test against, not simulated. Built once
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
                 \x20     id: bigint\n\
                 \x20     val: string\n\
                 \x20   bind: raw\n\
                 sources:\n\
                 \x20 raw: ./data.csv\n\
                 access:\n\
                 \x20 shareable: true\n",
            )
            .unwrap();
            std::fs::write(
                dir.join("sql/stg.sql"),
                "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, val)",
            )
            .unwrap();
            std::fs::write(dir.join("data.csv"), "id,val\n1,a\n2,b\n").unwrap();
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
            .expect("build the mixed (materializing + bound) scaffold cell");
            Scaffold { dir }
        })
    }

    fn never_router_with(mutate: impl FnOnce(&mut engine::Cell)) -> Router {
        let scaffold = built_never_cell();
        let cell_yaml = scaffold.dir.join("cell.yaml");
        let mut cell =
            engine::open(&cell_yaml, "local", true).expect("open built never-cell read-only");
        mutate(&mut cell);
        let (state, _store) =
            build_state(cell, /* no_data */ false, &cell_yaml, "local", "").expect("build state");
        app(state, DEFAULT_MAX_CONCURRENCY)
    }

    /// An ALL-BOUND cell (issue #6, issue #17): every export is bound, so
    /// `mounted` is empty regardless of `--no-data`. No `engine::run` here,
    /// unlike `built_never_cell` — `run` refuses to build a cell with no
    /// materializing transforms (no snapshot to commit); `engine::open`
    /// tolerates the missing/never-published catalog for exactly this cell
    /// class, so opening it read-only is enough to serve it.
    fn built_all_never_cell() -> &'static Scaffold {
        use std::sync::OnceLock;
        static CELL: OnceLock<Scaffold> = OnceLock::new();
        CELL.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!(
                "datamk-serve-smoke-all-never-{}-{}",
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
                "cell: virtual_only_smoke\n\
                 interface:\n\
                 \x20 - name: virtual_pii\n\
                 \x20   version: 1.0.0\n\
                 \x20   description: PII rows datamk verifies but never stores.\n\
                 \x20   grain: [id]\n\
                 \x20   schema:\n\
                 \x20     id: bigint\n\
                 \x20     val: string\n\
                 \x20   bind: raw\n\
                 sources:\n\
                 \x20 raw: ./data.csv\n\
                 access:\n\
                 \x20 shareable: true\n",
            )
            .unwrap();
            std::fs::write(dir.join("data.csv"), "id,val\n1,a\n2,b\n").unwrap();
            std::fs::write(
                dir.join("profiles/local.yaml"),
                "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
            )
            .unwrap();
            Scaffold { dir }
        })
    }

    /// The same all-`never` shape as `built_all_never_cell`, but with
    /// `datamk verify` actually run once at fixture-build time — so
    /// `.cell/source_check.json` exists and is fresh (issue #16). A
    /// separate fixture, not a mutation of `built_all_never_cell`: that one
    /// is relied on elsewhere (issue #17's tests, `two_doors`'s own
    /// draft-case test) to stay in the unverified, `draft` state.
    fn built_all_never_cell_verified() -> &'static Scaffold {
        use std::sync::OnceLock;
        static CELL: OnceLock<Scaffold> = OnceLock::new();
        CELL.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!(
                "datamk-serve-smoke-all-never-verified-{}-{}",
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
                "cell: virtual_verified_smoke\n\
                 interface:\n\
                 \x20 - name: virtual_pii\n\
                 \x20   version: 1.0.0\n\
                 \x20   description: PII rows datamk verifies but never stores.\n\
                 \x20   grain: [id]\n\
                 \x20   schema:\n\
                 \x20     id: bigint\n\
                 \x20     val: string\n\
                 \x20   bind: raw\n\
                 sources:\n\
                 \x20 raw: ./data.csv\n\
                 access:\n\
                 \x20 shareable: true\n",
            )
            .unwrap();
            std::fs::write(dir.join("data.csv"), "id,val\n1,a\n2,b\n").unwrap();
            std::fs::write(
                dir.join("profiles/local.yaml"),
                "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
            )
            .unwrap();
            crate::verify::run(&dir.join("cell.yaml"), "local")
                .expect("live-verify the all-bound scaffold, writing .cell/source_check.json");
            Scaffold { dir }
        })
    }

    fn all_never_router_with(mutate: impl FnOnce(&mut engine::Cell)) -> Router {
        let scaffold = built_all_never_cell();
        let cell_yaml = scaffold.dir.join("cell.yaml");
        let mut cell =
            engine::open(&cell_yaml, "local", true).expect("open built all-never cell read-only");
        mutate(&mut cell);
        let (state, _store) =
            build_state(cell, /* no_data */ false, &cell_yaml, "local", "").expect("build state");
        app(state, DEFAULT_MAX_CONCURRENCY)
    }

    fn all_never_verified_router_with(mutate: impl FnOnce(&mut engine::Cell)) -> Router {
        let scaffold = built_all_never_cell_verified();
        let cell_yaml = scaffold.dir.join("cell.yaml");
        let mut cell = engine::open(&cell_yaml, "local", true)
            .expect("open built all-never-verified cell read-only");
        mutate(&mut cell);
        let (state, _store) =
            build_state(cell, /* no_data */ false, &cell_yaml, "local", "").expect("build state");
        app(state, DEFAULT_MAX_CONCURRENCY)
    }

    /// The same scaffold as `built_all_never_cell_verified`, plus a
    /// `.cell/source_descriptions.json` (issue #6/#10) written directly via
    /// `SourceDescriptionsRecord::write` rather than a live BigQuery round
    /// trip — this fixture's `bind: raw` source is a local CSV, which never
    /// populates `SourceWarehouseColumns` (only a BigQuery `Connection`
    /// source's classify job does), so there is no credential-free way to
    /// produce this file through the real bind path. What's under test here
    /// is not the BigQuery metadata job (already pinned in
    /// `bigquery.rs`'s own unit tests) but the loading/serving wiring: both
    /// doors reading the same file through the same `fresh_for` gate and
    /// agreeing on the bound exports' warehouse descriptions.
    fn built_all_never_cell_with_descriptions() -> &'static Scaffold {
        use std::sync::OnceLock;
        static CELL: OnceLock<Scaffold> = OnceLock::new();
        CELL.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!(
                "datamk-serve-smoke-all-never-descriptions-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(dir.join("sql")).unwrap();
            std::fs::create_dir_all(dir.join("profiles")).unwrap();
            let cell_yaml = dir.join("cell.yaml");
            std::fs::write(
                &cell_yaml,
                "cell: virtual_descriptions_smoke\n\
                 interface:\n\
                 \x20 - name: virtual_pii\n\
                 \x20   version: 1.0.0\n\
                 \x20   description: PII rows datamk verifies but never stores.\n\
                 \x20   grain: [id]\n\
                 \x20   schema:\n\
                 \x20     id: bigint\n\
                 \x20     val: string\n\
                 \x20   bind: raw\n\
                 sources:\n\
                 \x20 raw: ./data.csv\n\
                 access:\n\
                 \x20 shareable: true\n",
            )
            .unwrap();
            std::fs::write(dir.join("data.csv"), "id,val\n1,a\n2,b\n").unwrap();
            std::fs::write(
                dir.join("profiles/local.yaml"),
                "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
            )
            .unwrap();
            crate::verify::run(&cell_yaml, "local")
                .expect("live-verify the all-bound scaffold, writing .cell/source_check.json");

            let cell_yaml_digest = crate::context::cell_yaml_digest_of(&cell_yaml).unwrap();
            let mut warehouse_columns = std::collections::HashMap::new();
            warehouse_columns.insert(
                "raw".to_string(),
                crate::engine::SourceWarehouseColumns {
                    connector: "bigquery",
                    columns: indexmap::IndexMap::new(),
                    descriptions: indexmap::IndexMap::from([(
                        "val".to_string(),
                        "The upstream value column, verbatim from the source system.".to_string(),
                    )]),
                },
            );
            let def = crate::config::CellDef::load(&cell_yaml).unwrap();
            crate::manifest::SourceDescriptionsRecord::write(
                &dir,
                &cell_yaml_digest,
                "local",
                &def,
                &warehouse_columns,
            )
            .expect("write .cell/source_descriptions.json");

            Scaffold { dir }
        })
    }

    fn all_never_with_descriptions_router_with(mutate: impl FnOnce(&mut engine::Cell)) -> Router {
        let scaffold = built_all_never_cell_with_descriptions();
        let cell_yaml = scaffold.dir.join("cell.yaml");
        let mut cell = engine::open(&cell_yaml, "local", true)
            .expect("open built all-never-with-descriptions cell read-only");
        mutate(&mut cell);
        let (state, _store) =
            build_state(cell, /* no_data */ false, &cell_yaml, "local", "").expect("build state");
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
            assert_eq!(v["datamk_context"], 4);
            assert_eq!(v["cell"], "smoke");
            assert_eq!(v["status"], "draft");
            assert_eq!(v["grain_verified"], false);
            assert!(v.get("build").is_none(), "{body}");
            assert_eq!(v["exports"][0]["route"], "orders_daily@2");
            assert_eq!(
                v["exports"][0]["query"]["sample_request"],
                "orders_daily@2?limit=10"
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
            let probe = &export_doc(&v, "orders_daily@2")["probe"];
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
                "orders_daily@2?order_date=2026-06-01&region=us-east&limit=10"
            );
            // The affordance is document-relative (ADR 0014): resolved
            // against this cell's own `/context`, which is at the root
            // here, that is a leading slash away.
            let (status, rows) = get(&router, &format!("/{example}"), None).await;
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
            // ADR 0012 §2 (amended): `query` is unconditional interface
            // grammar, not a claim about whether this surface currently
            // mounts the route — that claim is `data.served_here` (asserted
            // above) and the route's own 404, not a second, digest-moving
            // copy of the same fact.
            assert!(v["exports"][0]["query"].is_object(), "{body}");
            let probe = &export_doc(&v, "orders_daily@2")["probe"];
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

            // OpenAPI describes the callable surface (ADR 0013 §8: meta
            // paths always present) — no data paths, since none are mounted.
            let (status, body) = get(&router, "/openapi.json", None).await;
            assert_eq!(status, StatusCode::OK);
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            let paths = v["paths"].as_object().unwrap();
            assert!(paths.contains_key("/"), "{body}");
            assert!(paths.contains_key("/context"), "{body}");
            assert!(paths.contains_key("/openapi.json"), "{body}");
            assert!(!paths.contains_key("/orders_daily@2"), "{body}");
        });
    }

    /// ADR 0012 §2 (amended, commit 4): `query` is interface grammar, not
    /// mount state — identical whether or not `--no-data` was passed, for
    /// the same cell content. `data.served_here` legitimately still differs
    /// (it's the one honest "mounted here" fact, and `data` has always been
    /// part of `interface_digest` — `channels` too); what's fixed is that
    /// `query` no longer duplicates that fact a second, digest-moving way.
    #[test]
    fn query_grammar_is_stable_across_the_no_data_flag() {
        let with_data = router_mode(false, |_| {});
        let without_data = router_mode(true, |_| {});
        rt().block_on(async {
            let (_, body_a) = get(&with_data, "/context", None).await;
            let (_, body_b) = get(&without_data, "/context", None).await;
            let a: serde_json::Value = serde_json::from_str(&body_a).unwrap();
            let b: serde_json::Value = serde_json::from_str(&body_b).unwrap();
            // The fact that changed with the flag: still does.
            assert_ne!(a["data"]["served_here"], b["data"]["served_here"]);
            // The fact that must NOT change with the flag anymore: doesn't.
            assert!(a["exports"][0]["query"].is_object(), "{body_a}");
            assert_eq!(
                a["exports"][0]["query"], b["exports"][0]["query"],
                "query must not depend on --no-data: {body_a} / {body_b}"
            );
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

    // --- issue #6: bound exports — described, not routed --------------------

    /// ADR 0012 §4 amendment (2026-08-27): one export's slice of the
    /// document, same shape, own ETag variant, 404 naming the real routes.
    #[test]
    fn context_export_narrows_the_document_to_one_export() {
        let router = router_with(|_| {});
        rt().block_on(async {
            let (status, body) = get(&router, "/context/orders_daily@2", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["datamk_context"], 4);
            assert_eq!(v["exports"].as_array().unwrap().len(), 1);
            assert_eq!(v["exports"][0]["route"], "orders_daily@2");
            assert_eq!(v["cell"], "smoke");

            let (status, body) = get(&router, "/context/nope@1", None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
            assert!(body.contains("orders_daily@2"), "{body}");

            let (status, body) = get(&router, "/context/orders_daily@2?include=dcos", None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

            // Own ETag, distinct from the whole document's; round-trips to 304.
            let (_, h_all, _) = get_with_headers(&router, "/context", &[]).await;
            let (_, h_one, _) = get_with_headers(&router, "/context/orders_daily@2", &[]).await;
            let etag_all = h_all
                .get(header::ETAG)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            let etag_one = h_one
                .get(header::ETAG)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            assert_ne!(etag_all, etag_one);
            assert!(etag_one.contains("~export.orders_daily@2"), "{etag_one}");
            let (status, _, _) = get_with_headers(
                &router,
                "/context/orders_daily@2",
                &[("if-none-match", &etag_one)],
            )
            .await;
            assert_eq!(status, StatusCode::NOT_MODIFIED);

            // Advertised per export in the spec.
            let (_, body) = get(&router, "/openapi.json", None).await;
            let spec: serde_json::Value = serde_json::from_str(&body).unwrap();
            let route_enum =
                &spec["paths"]["/context/{route}"]["get"]["parameters"][0]["schema"]["enum"];
            assert_eq!(route_enum, &serde_json::json!(["orders_daily@2"]), "{body}");
        });
    }

    #[test]
    fn never_backed_export_is_declared_with_null_query_and_absent_from_openapi() {
        let router = never_router_with(|_| {});
        rt().block_on(async {
            let (status, body) = get(&router, "/context", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            let exports = v["exports"].as_array().unwrap();
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
                export_doc(&v, "virtual_pii@1").get("probe").is_none(),
                "{body}"
            );
            let stg_probe = &export_doc(&v, "stg@1")["probe"];
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
            assert!(body.contains("is `bind`ed to an existing object"), "{body}");
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
                !body.contains("is `bind`ed to an existing object"),
                "an unauthenticated caller must not learn this export is virtual: {body}"
            );

            // A correctly-authorized caller reaches the never-backed 404.
            let (status, body) = get(&router, "/virtual_pii@1", Some("good")).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
            assert!(body.contains("is `bind`ed to an existing object"), "{body}");
        });
    }

    /// issue #17: an ALL-`never` cell served WITHOUT `--no-data` still
    /// reports `served_here: false` and explains why with a definitional
    /// note (not the flag-derived `NOTE_NO_DATA`, since the operator never
    /// passed a flag here) — the exact reproduction from the filed issue.
    #[test]
    fn all_never_cell_served_here_is_false_without_no_data_flag() {
        let router = all_never_router_with(|_| {});
        rt().block_on(async {
            let (status, body) = get(&router, "/context", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["data"]["served_here"], false, "{body}");
            let notes = v["notes"].as_array().unwrap();
            assert!(
                notes
                    .iter()
                    .any(|n| n.as_str() == Some(crate::context::NOTE_NO_ROUTES_MOUNTED)),
                "expected the definitional note: {body}"
            );
            assert!(
                !notes
                    .iter()
                    .any(|n| n.as_str() == Some(crate::context::NOTE_NO_DATA)),
                "NOTE_NO_DATA is flag-derived and must not fire when --no-data was never \
                 passed: {body}"
            );

            // The data route itself already 404s (unchanged, issue #6's
            // `bound_routes` gate) — `/openapi.json` must agree that
            // no data path is mounted.
            let (status, body) = get(&router, "/openapi.json", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            let paths = v["paths"].as_object().unwrap();
            assert!(paths.contains_key("/"), "{body}");
            assert!(paths.contains_key("/context"), "{body}");
            assert!(paths.contains_key("/openapi.json"), "{body}");
            assert!(!paths.contains_key("/virtual_pii@1"), "{body}");
        });
    }

    /// issue #16: the exact filed reproduction. A `datamk verify` that
    /// live-checks an all-`never` cell (writing `.cell/source_check.json`)
    /// must make the **hosted** `/context` report `verified_at_source` too
    /// — not just the portable `datamk context` — since a virtual cell has
    /// no `run` and therefore no other way to ever leave `draft`.
    #[test]
    fn all_never_cell_reports_verified_at_source_on_the_hosted_door_after_verify() {
        let router = all_never_verified_router_with(|_| {});
        rt().block_on(async {
            let (status, body) = get(&router, "/context", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["status"], "verified_at_source", "{body}");
            assert_eq!(v["grain_verified"], true, "{body}");
            assert_eq!(v["source_check"]["outcome"], "passed", "{body}");
            assert!(v["source_check"]["checked_at"].is_string(), "{body}");
            // The profile never rides the wire (it's a gate, not a fact).
            assert!(v["source_check"].get("profile").is_none(), "{body}");
            // The draft-only notes (NOTE_DIRECT_ATTACH/NOTE_VIRTUAL_CELL/
            // NOTE_NOTHING_BUILT) are gone now that status isn't Draft — but
            // NOTE_NO_ROUTES_MOUNTED (issue #17) is not a draft note, it's a
            // structural serving fact independent of status, and stays true
            // regardless of how verified this document is.
            let notes = v["notes"].as_array().unwrap();
            assert_eq!(notes.len(), 1, "{body}");
            assert_eq!(notes[0], crate::context::NOTE_NO_ROUTES_MOUNTED, "{body}");
        });
    }

    /// M2: `NOTE_NO_ROUTES_MOUNTED`'s wording asserts a specific cause
    /// ("every export is bound directly to an existing object") — a cell
    /// with zero *discoverable* exports at all (every one `visibility:
    /// private`) also reaches `!served_here` with nothing mounted, with
    /// nothing bound anywhere. The note must not fire and misdescribe such
    /// a cell as bound.
    #[test]
    fn no_routes_mounted_note_does_not_fire_for_an_all_private_cell() {
        let router = router_with(|cell| {
            cell.def.interface[0].visibility = crate::config::Visibility::Private;
        });
        rt().block_on(async {
            let (status, body) = get(&router, "/context", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["data"]["served_here"], false, "{body}");
            let notes = v["notes"].as_array().unwrap();
            assert!(
                !notes
                    .iter()
                    .any(|n| n.as_str() == Some(crate::context::NOTE_NO_ROUTES_MOUNTED)),
                "an all-private cell has no bound export to describe — the note must \
                 not fire: {body}"
            );
        });
    }

    /// issue #16's fail-closed gate, end to end: a record written under one
    /// profile must not validate a different one, even with a matching
    /// `cell.yaml` digest and the same directory.
    #[test]
    fn source_check_from_a_different_profile_is_not_honored() {
        let scaffold = built_all_never_cell_verified();
        std::fs::write(
            scaffold.dir.join("profiles/other.yaml"),
            "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
        )
        .unwrap();
        let cell_yaml = scaffold.dir.join("cell.yaml");
        let cell = engine::open(&cell_yaml, "other", true)
            .expect("open the same scaffold under a different profile");
        let (state, _store) =
            build_state(cell, /* no_data */ false, &cell_yaml, "other", "").expect("build state");
        let router = app(state, DEFAULT_MAX_CONCURRENCY);
        rt().block_on(async {
            let (status, body) = get(&router, "/context", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(
                v["status"], "draft",
                "a record verified under 'local' must not validate 'other': {body}"
            );
            assert!(v["observed"].is_null(), "{body}");
        });
    }

    // --- ADR 0013: long-form docs pages -------------------------------

    /// A second, docs-bearing scaffold (the `init` scaffold declares no
    /// `docs:` fields) — cell-level and one export-level page, built once
    /// per test binary run like `built_cell`.
    fn built_cell_with_docs() -> &'static Scaffold {
        use std::sync::OnceLock;
        static CELL: OnceLock<Scaffold> = OnceLock::new();
        CELL.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!(
                "datamk-serve-smoke-docs-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(dir.join("sql")).unwrap();
            std::fs::create_dir_all(dir.join("profiles")).unwrap();
            std::fs::create_dir_all(dir.join("docs")).unwrap();
            std::fs::write(
                dir.join("cell.yaml"),
                "cell: docs_demo\n\
                 description: A demo cell with docs pages.\n\
                 docs: docs/overview.md\n\
                 transforms:\n\
                 \x20 - sql/orders.sql\n\
                 interface:\n\
                 \x20 - name: orders\n\
                 \x20   version: 1.0.0\n\
                 \x20   description: One row per order.\n\
                 \x20   docs: docs/orders.md\n\
                 \x20   grain: [order_id]\n\
                 \x20   schema:\n\
                 \x20     order_id: bigint\n\
                 \x20     revenue: decimal\n\
                 access:\n\
                 \x20 shareable: true\n",
            )
            .unwrap();
            std::fs::write(
                dir.join("sql/orders.sql"),
                "SELECT * FROM (VALUES (CAST(1 AS BIGINT), 10.0), (CAST(2 AS BIGINT), 20.0)) \
                 AS t(order_id, revenue)",
            )
            .unwrap();
            std::fs::write(
                dir.join("docs/overview.md"),
                "# Docs demo\n\nWhat this cell is for, at length.",
            )
            .unwrap();
            std::fs::write(
                dir.join("docs/orders.md"),
                "# Orders\n\nOne row per order placed, explained at length.",
            )
            .unwrap();
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
            .expect("build the docs scaffold cell");
            Scaffold { dir }
        })
    }

    fn router_docs(no_data: bool, mutate: impl FnOnce(&mut engine::Cell)) -> Router {
        let scaffold = built_cell_with_docs();
        let cell_yaml = scaffold.dir.join("cell.yaml");
        let mut cell =
            engine::open(&cell_yaml, "local", true).expect("open docs scaffold read-only");
        mutate(&mut cell);
        let (state, _store) =
            build_state(cell, no_data, &cell_yaml, "local", "").expect("build state");
        app(state, DEFAULT_MAX_CONCURRENCY)
    }

    async fn get_with_headers(
        router: &Router,
        uri: &str,
        extra: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, String) {
        let mut req = Request::builder().uri(uri);
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        let resp = router
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    /// `?include=docs` inlines both declared pages, switches to the docs
    /// variant `ETag`, and the plain `GET /context` keeps the byte-identical
    /// default `ETag`. Both round-trip through `If-None-Match` to a real 304
    /// — a client holding a cached *plain* `ETag` and asking for
    /// `?include=docs` must NOT 304 (the exact bug the exact-match check
    /// fixes by construction, ADR 0013 §6).
    #[test]
    fn include_docs_inlines_content_with_a_variant_etag_and_round_trips() {
        let router = router_docs(false, |_| {});
        rt().block_on(async {
            let (status, headers, body) = get_with_headers(&router, "/context", &[]).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let plain_etag = headers
                .get(header::ETAG)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            assert!(!plain_etag.contains("~docs"), "{plain_etag}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["included"], serde_json::json!([]));
            assert!(
                v["docs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|d| d.get("content").is_none()),
                "{body}"
            );
            assert_eq!(
                headers.get(header::CACHE_CONTROL).unwrap(),
                "private",
                "{body}"
            );

            let (status, headers, body) =
                get_with_headers(&router, "/context?include=docs", &[]).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let docs_etag = headers
                .get(header::ETAG)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            assert!(docs_etag.contains("~docs."), "{docs_etag}");
            assert_ne!(docs_etag, plain_etag);

            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["included"], serde_json::json!(["docs"]));
            assert_eq!(
                docs_page(&v, "cell")["media_type"],
                "text/markdown; charset=utf-8"
            );
            assert!(
                docs_page(&v, "cell")["content"]
                    .as_str()
                    .unwrap()
                    .contains("Docs demo"),
                "{body}"
            );
            assert!(
                docs_page(&v, "orders@1")["content"]
                    .as_str()
                    .unwrap()
                    .contains("One row per order"),
                "{body}"
            );
            // Fingerprints ship in the default variant too (not gated on
            // `include=docs`) — a release ran for this scaffold (`engine::run`
            // in direct-attach mode still writes no `published.json` unless
            // `release` runs, so this cell has none; assert the shape stays
            // absent-not-fabricated instead).
            let (status, _headers, default_body) = get_with_headers(&router, "/context", &[]).await;
            assert_eq!(status, StatusCode::OK, "{default_body}");
            let dv: serde_json::Value = serde_json::from_str(&default_body).unwrap();
            assert!(
                dv["docs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|d| d.get("sha256").is_none()),
                "{default_body}"
            );

            // The plain ETag round-trips to 304; the docs ETag does too —
            // and a plain-cached client asking with `?include=docs` gets a
            // fresh 200, never a false 304.
            let (status, _, _) =
                get_with_headers(&router, "/context", &[("if-none-match", &plain_etag)]).await;
            assert_eq!(status, StatusCode::NOT_MODIFIED);
            let (status, _, _) = get_with_headers(
                &router,
                "/context?include=docs",
                &[("if-none-match", &docs_etag)],
            )
            .await;
            assert_eq!(status, StatusCode::NOT_MODIFIED);
            let (status, _, _) = get_with_headers(
                &router,
                "/context?include=docs",
                &[("if-none-match", &plain_etag)],
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "a plain cached ETag must not 304-match the docs variant"
            );
        });
    }

    /// H3: a cell with a live-verify record folds `~observed.<hash>` into
    /// the default `ETag` too, not just the docs variant — otherwise
    /// `interface_digest` (declared/data only) stays byte-identical across
    /// the rollout that first ships `source_check`/`source_descriptions`
    /// (`cell.yaml` is unchanged), and a caching client's `If-None-Match`
    /// would 304 straight past `status: verified_at_source` ever appearing.
    /// A cell with no observed input at all (`router_with`, no live-verify
    /// record) keeps the exact byte-identical plain digest as its ETag —
    /// the common case must not pay a suffix it has nothing to invalidate.
    #[test]
    fn observed_inputs_fold_into_the_default_etag_and_still_304_correctly() {
        let plain_router = router_with(|_| {});
        let verified_router = all_never_verified_router_with(|_| {});
        rt().block_on(async {
            let (status, headers, body) = get_with_headers(&plain_router, "/context", &[]).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let plain_etag = headers
                .get(header::ETAG)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            assert!(
                !plain_etag.contains("~observed."),
                "a cell with no live-verify record must keep its byte-identical \
                 default ETag: {plain_etag}"
            );

            let (status, headers, body) = get_with_headers(&verified_router, "/context", &[]).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let observed_etag = headers
                .get(header::ETAG)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            assert!(
                observed_etag.contains("~observed."),
                "a cell with a live-verify record must fold it into the default \
                 ETag: {observed_etag}"
            );

            // Round-trips to a real 304 on a match...
            let (status, _, _) = get_with_headers(
                &verified_router,
                "/context",
                &[("if-none-match", &observed_etag)],
            )
            .await;
            assert_eq!(status, StatusCode::NOT_MODIFIED);

            // ...and — the actual regression — a client holding only the
            // bare `interface_digest` as its cached ETag (what every prior
            // rollout would have handed it) must NOT 304-match once this
            // cell has a live-verify record: it would otherwise never learn
            // `status` moved to `verified_at_source`.
            let bare_digest_etag = format!(
                "\"{}\"",
                observed_etag.trim_matches('"').split('~').next().unwrap()
            );
            assert_ne!(bare_digest_etag, observed_etag);
            let (status, _, _) = get_with_headers(
                &verified_router,
                "/context",
                &[("if-none-match", &bare_digest_etag)],
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "a stale, observed-unaware cached ETag must never 304-match"
            );
        });
    }

    /// `?include=docs` on a cell with no `docs:` fields at all is a normal
    /// 200 with an empty pages map — not an error.
    #[test]
    fn include_docs_on_a_docs_less_cell_is_200_with_empty_pages() {
        let router = router_with(|_| {});
        rt().block_on(async {
            let (status, body) = get(&router, "/context?include=docs", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["included"], serde_json::json!(["docs"]));
            assert_eq!(v["docs"], serde_json::json!([]));
        });
    }

    /// The closed `include=` grammar (ADR 0013 §4): an unrecognized query
    /// parameter, an unrecognized section, and an empty/trailing-comma value
    /// are all 400s naming exactly what's accepted.
    #[test]
    fn context_query_param_validation_rejects_the_closed_set() {
        let router = router_with(|_| {});
        rt().block_on(async {
            let (status, body) = get(&router, "/context?limit=5", None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(body.contains("unknown query parameter 'limit'"), "{body}");
            assert!(body.contains("sections: docs"), "{body}");

            let (status, body) = get(&router, "/context?include=dcos", None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(body.contains("unknown `include` section 'dcos'"), "{body}");
            assert!(body.contains("accepts: docs"), "{body}");

            let (status, body) = get(&router, "/context?include=", None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(body.contains("must name at least one section"), "{body}");

            let (status, body) = get(&router, "/context?include=docs,", None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(body.contains("must name at least one section"), "{body}");

            // A repeated identical token is accepted.
            let (status, body) = get(&router, "/context?include=docs&include=docs", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        });
    }

    /// ADR 0013 §10: docs stay available under `--no-data` — the withheld
    /// `values` lists are row-derived; prose is not.
    #[test]
    fn no_data_mode_keeps_docs_available() {
        let router = router_docs(true, |_| {});
        rt().block_on(async {
            let (status, body) = get(&router, "/context?include=docs", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["data"]["served_here"], false);
            assert_eq!(v["included"], serde_json::json!(["docs"]));
            assert!(
                docs_page(&v, "cell")["content"]
                    .as_str()
                    .unwrap()
                    .contains("Docs demo"),
                "{body}"
            );
        });
    }

    /// The two-doors regression (ADR 0012 §4): `datamk context`
    /// (`context::build_document`) and hosted `GET /context` are two doors
    /// onto the same cell — every divergence between them that isn't
    /// explicitly documented here is a bug, not a feature. Compares the two
    /// **live-computed** documents against each other on real fixtures,
    /// never against a frozen golden string: an additive, shape-preserving
    /// change to either representation survives this test as long as both
    /// doors change together; a real divergence (like issue #16/#17 before
    /// they landed) fails it.
    mod two_doors {
        use super::*;

        /// The hosted door: `GET /context`, optionally requesting the docs
        /// variant — the exact router `serve` binds, no shortcuts.
        async fn hosted_doc(router: &Router, include_docs: bool) -> serde_json::Value {
            let uri = if include_docs {
                "/context?include=docs"
            } else {
                "/context"
            };
            let (status, body) = get(router, uri, None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            serde_json::from_str(&body).unwrap()
        }

        /// The portable door: `context::build_document` against the exact
        /// same scaffold directory the router was built from — the same
        /// entry point `datamk context` calls, minus the stdout/`--out`
        /// choice `emit` layers on top (commit 5's extraction).
        fn portable_doc(scaffold: &Scaffold, no_docs: bool) -> serde_json::Value {
            let doc =
                crate::context::build_document(&scaffold.dir.join("cell.yaml"), "local", no_docs)
                    .expect("build_document");
            serde_json::to_value(&doc).unwrap()
        }

        /// Assert the two documents agree on everything **except** the
        /// asymmetries this ADR documents as permanent, not as gaps to
        /// close — asserting each one's documented shape *before* removing
        /// it, so a change to which fields are asymmetric is itself a
        /// visible test failure, not a silent pass:
        ///
        /// - `emitted_at` / `cell_yaml_digest`: portable-only (ADR 0012 §4)
        ///   — a hosted response is always "now," a file is not.
        /// - `data.served_here`: portable is unconditionally `false` (a
        ///   file serves no rows, by definition); hosted's real value is
        ///   `serve`'s own concern (issue #17's tests), not re-litigated
        ///   here.
        /// - `observed.exports` (swap-time probes): portable never runs
        ///   them — `build_document` calls `config::load`, never
        ///   `engine::open`, so there is no DuckDB connection to probe with.
        ///   Found by this test: stripping `exports` alone left hosted's
        ///   `observed` as `{"provenance": null}` (a present-but-empty
        ///   object — `Observed.provenance` has no `skip_serializing_if`,
        ///   ADR 0012 §5's "always show provenance, never omit it") against
        ///   portable's `observed: null` — a real shape difference that is
        ///   an artifact of comparison (probes are the only thing that made
        ///   `observed` non-`None` on either side here), not a bug in
        ///   either door, so both are collapsed to `null` together when
        ///   nothing but `null`s remains.
        /// - `observed.freshness`: portable never carries poll telemetry
        ///   (a lie the instant the file is written); on these fixtures
        ///   (none published-mode) hosted has none either, but the check
        ///   stays generic rather than assuming that forever.
        /// - `notes`: hosted alone can carry `NOTE_NO_ROUTES_MOUNTED`
        ///   (issue #17) — found by this test on the all-`never` fixture.
        ///   Portable has no analogous note because it never claims
        ///   `served_here: true` in the first place (unconditionally
        ///   `false`), so there is no "routes should be here but aren't" to
        ///   explain; only a *server* that could have mounted routes and
        ///   didn't needs the sentence.
        ///
        /// Everything else — `status`, `grain_verified`, `declared`
        /// (including `query`, unconditional since commit 4),
        /// `observed.provenance`/`source_check`, every other note,
        /// `included`/`docs` — is compared with full equality.
        ///
        /// `observed.source_check` populated (issue #16) IS covered —
        /// `virtual_cell_with_source_check_agrees_across_both_doors`, below
        /// — now that `serve`'s startup load makes both doors read the same
        /// `.cell/source_check.json` through the same `fresh_for` gate.
        fn assert_agree_modulo_documented_asymmetries(
            mut portable: serde_json::Value,
            mut hosted: serde_json::Value,
        ) {
            assert!(portable["emitted_at"].is_string(), "{portable}");
            assert!(hosted.get("emitted_at").is_none(), "{hosted}");
            assert!(portable["cell_yaml_digest"].is_string(), "{portable}");
            assert!(hosted.get("cell_yaml_digest").is_none(), "{hosted}");
            assert_eq!(portable["data"]["served_here"], false, "{portable}");
            assert!(
                portable["exports"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|e| e.get("probe").is_none()),
                "a file never probes rows: {portable}"
            );
            assert!(portable.get("freshness").is_none(), "{portable}");
            assert!(
                hosted.get("freshness").is_none(),
                "none of this test's fixtures are published-mode, so hosted should have no \
                 freshness block either — if this fires, add a real asymmetry check instead of \
                 stripping blind: {hosted}"
            );
            assert!(
                !portable["notes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|n| n.as_str() == Some(crate::context::NOTE_NO_ROUTES_MOUNTED)),
                "portable must never carry the routes-unmounted note — it never claims \
                 served_here: true to begin with: {portable}"
            );

            for doc in [&mut portable, &mut hosted] {
                let obj = doc.as_object_mut().expect("document is a JSON object");
                obj.remove("emitted_at");
                obj.remove("cell_yaml_digest");
                if let Some(data) = obj.get_mut("data").and_then(|d| d.as_object_mut()) {
                    data.remove("served_here");
                }
                obj.remove("freshness");
                if let Some(exports) = obj.get_mut("exports").and_then(|e| e.as_array_mut()) {
                    for e in exports.iter_mut() {
                        if let Some(e) = e.as_object_mut() {
                            e.remove("probe");
                        }
                    }
                }
                if let Some(notes) = obj.get_mut("notes").and_then(|n| n.as_array_mut()) {
                    notes.retain(|n| n.as_str() != Some(crate::context::NOTE_NO_ROUTES_MOUNTED));
                }
            }

            assert_eq!(
                portable, hosted,
                "the two doors disagree outside the documented asymmetries"
            );
        }

        /// Runs both inclusion variants (default / `?include=docs`) for one
        /// scaffold + router pair — like-for-like: portable's `no_docs` is
        /// the inverse of whether docs were requested, so "default" compares
        /// against "default" and "with docs" against "with docs," never
        /// portable's default (inline) against hosted's default (omit),
        /// which differ by ADR 0013 §7 design and would be a false failure.
        fn check_both_doors(scaffold: &'static Scaffold, router: Router) {
            rt().block_on(async {
                for include_docs in [false, true] {
                    let portable = portable_doc(scaffold, /* no_docs */ !include_docs);
                    let hosted = hosted_doc(&router, include_docs).await;
                    assert_agree_modulo_documented_asymmetries(portable, hosted);
                }
            });
        }

        #[test]
        fn materializing_cell_agrees_across_both_doors() {
            check_both_doors(built_cell(), router_with(|_| {}));
        }

        #[test]
        fn mixed_never_cell_agrees_across_both_doors() {
            check_both_doors(built_never_cell(), never_router_with(|_| {}));
        }

        #[test]
        fn all_never_cell_agrees_across_both_doors() {
            check_both_doors(built_all_never_cell(), all_never_router_with(|_| {}));
        }

        /// The fixture the previous version of this test's doc comment said
        /// was missing (issue #16): a live-verified all-`never` cell, where
        /// `observed.source_check` is actually populated on both doors.
        /// Both must reach `status: verified_at_source` from the exact same
        /// `.cell/source_check.json`, gated through the exact same
        /// `SourceCheckRecord::fresh_for`.
        #[test]
        fn virtual_cell_with_source_check_agrees_across_both_doors() {
            check_both_doors(
                built_all_never_cell_verified(),
                all_never_verified_router_with(|_| {}),
            );
        }

        /// Issue #6/#10: `observed.source_descriptions`, populated on both
        /// doors from the exact same `.cell/source_descriptions.json`
        /// through the exact same `SourceDescriptionsRecord::fresh_for` gate
        /// `source_check` already proves out above — no asymmetry allowance
        /// needed, since `assert_agree_modulo_documented_asymmetries` only
        /// strips fields with a *documented* reason to differ, and there is
        /// none here.
        #[test]
        fn virtual_cell_with_source_descriptions_agrees_across_both_doors() {
            check_both_doors(
                built_all_never_cell_with_descriptions(),
                all_never_with_descriptions_router_with(|_| {}),
            );
        }
    }

    /// `run_project`'s router (ADR 0014): N cells nested under `/<mount>`,
    /// driven through `project_router` directly — same `oneshot`, no-socket
    /// idiom as the rest of this suite.
    mod project_mode {
        use super::*;

        /// `test/integrations/orders`, copied to scratch and built fresh —
        /// a second real cell, independent of `built_cell()`'s init
        /// scaffold, so multi-cell tests never depend on committed `.cell`
        /// state.
        fn built_orders_cell() -> &'static Scaffold {
            use std::sync::OnceLock;
            static CELL: OnceLock<Scaffold> = OnceLock::new();
            CELL.get_or_init(|| {
                let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("test/integrations/orders");
                let dir = std::env::temp_dir().join(format!(
                    "datamk-serve-smoke-orders-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                copy_cell_dir(&src, &dir);
                crate::engine::run(
                    &dir.join("cell.yaml"),
                    "local",
                    None,
                    crate::engine::RunOptions::default(),
                )
                .expect("build the orders fixture cell");
                Scaffold { dir }
            })
        }

        fn copy_cell_dir(src: &Path, dst: &Path) {
            std::fs::create_dir_all(dst).unwrap();
            for entry in std::fs::read_dir(src).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name();
                if name == ".cell" {
                    continue; // generated state — `engine::run` produces its own
                }
                let (from, to) = (entry.path(), dst.join(&name));
                if entry.file_type().unwrap().is_dir() {
                    copy_cell_dir(&from, &to);
                } else {
                    std::fs::copy(&from, &to).unwrap();
                }
            }
        }

        /// Open `scaffold` read-only, mutate the parsed definition, and
        /// build the mounted `(mount, state)` pair `project_router` takes.
        fn mounted(
            scaffold: &Scaffold,
            mount: &str,
            no_data: bool,
            mutate: impl FnOnce(&mut engine::Cell),
        ) -> (String, Arc<AppState>) {
            let cell_yaml = scaffold.dir.join("cell.yaml");
            let mut cell = engine::open(&cell_yaml, "local", true).expect("open cell read-only");
            mutate(&mut cell);
            let base_path = format!("/{mount}");
            let (state, _store) =
                build_state(cell, no_data, &cell_yaml, "local", &base_path).expect("build state");
            (mount.to_string(), state)
        }

        #[test]
        fn each_mount_serves_its_own_context_with_its_own_digest() {
            let router = project_router(
                vec![
                    mounted(built_cell(), "a", false, |_| {}),
                    mounted(built_orders_cell(), "b", false, |_| {}),
                ],
                DEFAULT_MAX_CONCURRENCY,
            );
            rt().block_on(async {
                let (status, headers_a, body_a) =
                    get_with_headers(&router, "/a/context", &[]).await;
                assert_eq!(status, StatusCode::OK, "{body_a}");
                let va: serde_json::Value = serde_json::from_str(&body_a).unwrap();
                assert_eq!(va["cell"], "smoke");

                let (status, headers_b, body_b) =
                    get_with_headers(&router, "/b/context", &[]).await;
                assert_eq!(status, StatusCode::OK, "{body_b}");
                let vb: serde_json::Value = serde_json::from_str(&body_b).unwrap();
                assert_eq!(vb["cell"], "orders");

                assert_ne!(
                    headers_a.get(header::ETAG),
                    headers_b.get(header::ETAG),
                    "two different cells must not share a digest"
                );
            });
        }

        /// The whole point of relative request affordances (ADR 0014,
        /// `datamk_context: 3`): the same cell's digest and declared
        /// document do not change when it moves from the root to a mount.
        #[test]
        fn a_cells_digest_is_identical_mounted_and_unmounted() {
            // Unmounted: the exact single-cell router `run` binds, base_path "".
            let router_unmounted = router_with(|_| {});
            let router_mounted = project_router(
                vec![mounted(built_cell(), "weather", false, |_| {})],
                DEFAULT_MAX_CONCURRENCY,
            );
            rt().block_on(async {
                let (_, headers_u, body_u) =
                    get_with_headers(&router_unmounted, "/context", &[]).await;
                let (_, headers_m, body_m) =
                    get_with_headers(&router_mounted, "/weather/context", &[]).await;
                assert_eq!(
                    headers_u.get(header::ETAG),
                    headers_m.get(header::ETAG),
                    "mounting must not move the digest"
                );
                let vu: serde_json::Value = serde_json::from_str(&body_u).unwrap();
                let vm: serde_json::Value = serde_json::from_str(&body_m).unwrap();
                assert_eq!(vu["declared"], vm["declared"], "{body_u}\n{body_m}");
                assert_eq!(
                    vu["exports"][0]["query"]["sample_request"],
                    vm["exports"][0]["query"]["sample_request"],
                    "the sample request string itself must not change"
                );
            });
        }

        /// An anonymous caller against a project with one open and one
        /// role-gated cell sees only the one it can actually reach —
        /// `status` stays `ok` regardless (ADR 0012 §6: `/` is a liveness
        /// route, never a served mesh manifest).
        #[test]
        fn root_lists_only_mounts_the_caller_can_reach() {
            let principals = built_cell().dir.join("principals_project_mode.json");
            std::fs::write(&principals, r#"{ "good": ["analyst"] }"#).unwrap();
            let gated = mounted(built_cell(), "gated", false, |cell| {
                cell.def.access.roles = vec!["analyst".to_string()];
                cell.principals = Some(principals.to_string_lossy().into_owned());
            });
            let open = mounted(built_cell(), "open", false, |_| {});
            let router = project_router(vec![open, gated], DEFAULT_MAX_CONCURRENCY);

            rt().block_on(async {
                let (status, body) = get(&router, "/", None).await;
                assert_eq!(status, StatusCode::OK, "{body}");
                let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(v["status"], "ok");
                assert_eq!(v["cells"], serde_json::json!(["open"]), "{body}");

                let (status, body) = get(&router, "/", Some("good")).await;
                assert_eq!(status, StatusCode::OK, "{body}");
                let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(v["status"], "ok");
                let cells: std::collections::BTreeSet<&str> = v["cells"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| c.as_str().unwrap())
                    .collect();
                assert_eq!(
                    cells,
                    ["gated", "open"].into_iter().collect(),
                    "an analyst token sees both mounts: {body}"
                );
            });
        }

        #[test]
        fn unknown_mount_404s_naming_the_door_that_exists() {
            let router = project_router(
                vec![mounted(built_cell(), "a", false, |_| {})],
                DEFAULT_MAX_CONCURRENCY,
            );
            rt().block_on(async {
                let (status, body) = get(&router, "/nope", None).await;
                assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
                assert!(body.contains("/<cell>/context"), "{body}");
            });
        }

        /// `--no-data` (process-wide) and a per-cell `no_data:` (project
        /// file) union rather than override one another.
        #[test]
        fn no_data_unions_the_process_flag_and_the_per_cell_flag() {
            let router = project_router(
                vec![
                    mounted(built_cell(), "a", /* process --no-data */ true, |_| {}),
                    mounted(built_cell(), "b", /* process --no-data */ true, |_| {}),
                ],
                DEFAULT_MAX_CONCURRENCY,
            );
            rt().block_on(async {
                for mount in ["a", "b"] {
                    let (status, body) =
                        get(&router, &format!("/{mount}/orders_daily@2"), None).await;
                    assert_eq!(
                        status,
                        StatusCode::NOT_FOUND,
                        "--no-data must apply to every mounted cell: {body}"
                    );
                }
            });
        }

        /// axum 0.7's `nest`: a request to the mount with **no** trailing
        /// slash must still reach the nested router's `/` — every mesh
        /// `url` and every relative affordance is written without one.
        #[test]
        fn nest_without_a_trailing_slash_still_reaches_the_inner_root() {
            let router = project_router(
                vec![mounted(built_cell(), "weather", false, |_| {})],
                DEFAULT_MAX_CONCURRENCY,
            );
            rt().block_on(async {
                let (status, body) = get(&router, "/weather", None).await;
                assert_eq!(status, StatusCode::OK, "{body}");
                let (status, body) = get(&router, "/weather/", None).await;
                assert_eq!(status, StatusCode::OK, "{body}");
            });
        }

        /// The `Link` back-link and the context digest header must resolve
        /// under a mount on error responses too, not just 200 — a caller
        /// debugging a 400/404 needs the same affordance a 200 gets.
        #[test]
        fn link_and_digest_headers_resolve_under_a_mount_on_400_and_404() {
            let router = project_router(
                vec![mounted(built_cell(), "weather", false, |_| {})],
                DEFAULT_MAX_CONCURRENCY,
            );
            rt().block_on(async {
                // 400: an unknown query param on a real data route.
                let (status, headers, body) =
                    get_with_headers(&router, "/weather/orders_daily@2?bogus=1", &[]).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
                let link = headers
                    .get(header::LINK)
                    .and_then(|v| v.to_str().ok())
                    .expect("Link header present on a 400");
                assert_eq!(link, "</weather/context>; rel=\"describedby\"");
                assert!(headers.get("x-datamk-context-digest").is_some());

                // 404: an export that does not exist.
                let (status, headers, body) =
                    get_with_headers(&router, "/weather/nope@1", &[]).await;
                assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
                let link = headers
                    .get(header::LINK)
                    .and_then(|v| v.to_str().ok())
                    .expect("Link header present on a 404");
                assert_eq!(link, "</weather/context>; rel=\"describedby\"");
                assert!(headers.get("x-datamk-context-digest").is_some());
            });
        }

        /// A role-gated cell whose profile names a **relative** `principals:`
        /// path (`principals.json`, resolved against its own dir — the shape
        /// every real cell uses) and a token->role map containing exactly
        /// one, distinguishing token. Two of these mounted in one project is
        /// the regression fixture: pre-fix, `config::load` left the relative
        /// path unrebased, so every mounted cell's `cell.principals` was the
        /// literal `"principals.json"`, read against the server process's
        /// cwd instead of either cell's own directory.
        fn scaffold_gated_cell(name: &str, token: &str) -> Scaffold {
            let dir = std::env::temp_dir().join(format!(
                "datamk-serve-smoke-gated-{name}-{}-{}",
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
                format!(
                    "cell: {name}\n\
                     transforms:\n\
                     \x20 - sql/t.sql\n\
                     interface:\n\
                     \x20 - name: t\n\
                     \x20   version: 1.0.0\n\
                     \x20   grain: [id]\n\
                     \x20   schema:\n\
                     \x20     id: integer\n\
                     \x20     val: string\n\
                     access:\n\
                     \x20 shareable: true\n\
                     \x20 roles: [analyst]\n"
                ),
            )
            .unwrap();
            std::fs::write(
                dir.join("sql/t.sql"),
                "SELECT * FROM (VALUES (1, 'a')) AS t(id, val)",
            )
            .unwrap();
            std::fs::write(
                dir.join("profiles/local.yaml"),
                "catalog: ./.cell/catalog.ducklake\n\
                 storage: ./.cell/data\n\
                 principals: principals.json\n",
            )
            .unwrap();
            std::fs::write(
                dir.join("principals.json"),
                format!(r#"{{ "{token}": ["analyst"] }}"#),
            )
            .unwrap();
            crate::engine::run(
                &dir.join("cell.yaml"),
                "local",
                None,
                crate::engine::RunOptions::default(),
            )
            .expect("build the gated scaffold cell");
            Scaffold { dir }
        }

        /// The actual regression guard for the security bug fixed alongside
        /// this test: two cells, mounted in one project, whose profiles both
        /// point `principals:` at the same *relative* filename. Each cell's
        /// own token must authorize only that cell — never the other's, and
        /// never by falling back to whatever `principals.json` happens to be
        /// readable from the server process's cwd. Fails without the
        /// `config::load` rebase (either `build_state` errors because
        /// `principals.json` doesn't exist relative to the test binary's cwd,
        /// or — if it happened to exist there — both mounts would wrongly
        /// share it).
        #[test]
        fn each_mount_authorizes_only_its_own_principals_file() {
            let a = scaffold_gated_cell("gated_a", "token-a");
            let b = scaffold_gated_cell("gated_b", "token-b");
            let router = project_router(
                vec![
                    mounted(&a, "a", false, |_| {}),
                    mounted(&b, "b", false, |_| {}),
                ],
                DEFAULT_MAX_CONCURRENCY,
            );
            rt().block_on(async {
                let (status, body) = get(&router, "/a/t@1", Some("token-a")).await;
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "a's own token must work on a: {body}"
                );
                let (status, body) = get(&router, "/b/t@1", Some("token-b")).await;
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "b's own token must work on b: {body}"
                );

                let (status, _) = get(&router, "/a/t@1", Some("token-b")).await;
                assert_eq!(
                    status,
                    StatusCode::UNAUTHORIZED,
                    "b's token must not authorize a"
                );
                let (status, _) = get(&router, "/b/t@1", Some("token-a")).await;
                assert_eq!(
                    status,
                    StatusCode::UNAUTHORIZED,
                    "a's token must not authorize b"
                );
            });
        }
    }
}
