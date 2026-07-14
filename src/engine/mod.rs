mod connectors;
pub mod run_summary;

use anyhow::{Context, Result};
use duckdb::Connection;
use indexmap::IndexMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{
    CellDef, ConnectionTarget, MaterializeStrategy, ResolvedBindings, ResolvedConnection,
    ResolvedGcs, ResolvedIncremental, ResolvedS3, ResolvedSource, ResolvedTransform,
};
use crate::store::Store;
use crate::timeutil::rfc3339_utc;
use connectors::{ClassifyCache, CursorPredicate, ObjectKind, ObjectMeta};
use run_summary::{RunSummary, SourceRunInfo, TransformRunInfo};

/// An opened cell: parsed definition + a live DuckDB connection with DuckLake
/// attached as the schema `lake`.
pub struct Cell {
    pub def: CellDef,
    pub conn: Connection,
    /// Directory containing the cell definition; transforms and local bindings
    /// resolve relative to it.
    pub dir: PathBuf,
    /// Resolved sources, bound as TEMP VIEWs during `run`.
    pub sources: IndexMap<String, ResolvedSource>,
    /// `def.transforms`, validated and normalized (ADR 0008 work item 1) —
    /// the run loop and `verify_replay` dispatch on this, never on
    /// `def.transforms` directly.
    pub transforms: Vec<ResolvedTransform>,
    /// Resolved token->roles file path, if configured.
    pub principals: Option<String>,
    /// The profile's `s3:` block; drives both DuckDB's secret and the native
    /// object-store client (ADR 0004 §3 credential parity).
    pub s3: Option<ResolvedS3>,
    /// The profile's `gcs:` block. Unlike `s3:`, its two credential planes are
    /// split by necessity: `key_id`/`secret` (HMAC) drive DuckDB's secret,
    /// `credentials`/ADC drive the native object-store client.
    pub gcs: Option<ResolvedGcs>,
    /// Published-artifact mode state (ADR 0004): present iff the profile has
    /// no `catalog:`.
    pub published: Option<PublishedState>,
    /// Run-scoped local scratch (downloaded artifacts live here). Removed on
    /// drop.
    scratch: PathBuf,
}

/// Published-mode context: the cell's slice of the object store and the local
/// working copy of the catalog.
pub struct PublishedState {
    pub store: Arc<Store>,
    /// Local catalog file this connection is attached to.
    pub local: PathBuf,
    /// The execution the local copy came from (`None` = bootstrap: no
    /// published catalog existed yet; this run creates execution 1).
    pub execution: Option<u64>,
}

impl Drop for Cell {
    fn drop(&mut self) {
        // Best-effort scratch cleanup. The catalog file may still be attached
        // by `conn` at this point; unlinking an open file is fine on unix.
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

/// Flags that change what `run` does, orthogonal to the profile/retention
/// knobs it already takes (ADR 0005 §3, §2 item 1).
#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    /// Bind every incremental source unfiltered and rewrite its watermark to
    /// the fresh `max(cursor)` at commit. No-op on a cell with no incremental
    /// sources.
    pub full_refresh: bool,
    /// After transforms succeed, replay them once against the same staged
    /// delta inside a transaction that is then rolled back, and fail if any
    /// output table's contents changed. No-op on a cell with no incremental
    /// sources.
    pub verify_replay: bool,
}

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Process id + a monotonic counter: unique per call within this process,
/// and — because it carries the process id — an orphan left behind by a
/// crash is identifiable as "this run" after the fact. Shared by every
/// run-scoped scratch name this engine hands out: the local `new_scratch_dir`
/// below, and the ADR 0006 §3a `EXPORT DATA` run prefix (`stage_via_export`),
/// so both disciplines can never drift apart.
fn unique_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// A filesystem/URI-safe rendering of an arbitrary tag (a cell name, a
/// source name): ASCII alphanumerics and `-` pass through, everything else
/// becomes `-`.
fn sanitize_tag(tag: &str) -> String {
    tag.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// A unique, process-scoped local scratch directory.
fn new_scratch_dir(tag: &str) -> Result<PathBuf> {
    let dir =
        std::env::temp_dir().join(format!("datamk-{}-{}", sanitize_tag(tag), unique_suffix()));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating scratch dir {}", dir.display()))?;
    Ok(dir)
}

/// An in-memory DuckDB connection, configured for the profile. A native GCS
/// extension (`gcs.extension`) is a locally-built binary outside DuckDB's
/// signing chain, so that one case opts the connection into unsigned
/// extensions — a connection-open flag, immutable afterwards.
fn open_duckdb(gcs: Option<&ResolvedGcs>) -> Result<Connection> {
    if gcs.is_some_and(|g| g.extension.is_some()) {
        let config = duckdb::Config::default()
            .allow_unsigned_extensions()
            .context("enabling unsigned extensions for gcs.extension")?;
        Connection::open_in_memory_with_flags(config).context("opening DuckDB")
    } else {
        Connection::open_in_memory().context("opening DuckDB")
    }
}

pub fn open(file: &Path, profile: &str, read_only: bool) -> Result<Cell> {
    // The pure parse+resolve prefix lives in `config::load` (no DB); `open` is
    // that plus a connection. `deploy` uses `config::load` directly to inspect a
    // cell without ever opening a database.
    let loaded = crate::config::load(file, profile)?;
    let conn = open_duckdb(loaded.bindings.gcs.as_ref())?;
    let scratch = new_scratch_dir(&loaded.def.cell)?;
    let published = setup(&conn, &loaded.bindings, &loaded.dir, read_only, &scratch)?;
    Ok(Cell {
        def: loaded.def,
        conn,
        dir: loaded.dir,
        sources: loaded.bindings.sources.clone(),
        transforms: loaded.transforms,
        principals: loaded.bindings.principals.clone(),
        s3: loaded.bindings.s3.clone(),
        gcs: loaded.bindings.gcs.clone(),
        published,
        scratch,
    })
}

fn setup(
    conn: &Connection,
    b: &ResolvedBindings,
    dir: &Path,
    read_only: bool,
    scratch: &Path,
) -> Result<Option<PublishedState>> {
    // INSTALL fetches the extension from the registry on first run (needs network).
    // SET TimeZone: DuckDB's session zone follows the OS locale by default, so
    // TIMESTAMPTZ rendering (watermark literals, ops output) would vary by host
    // — a laptop in Chicago renders `-05` where a UTC pod renders `Z`. Pinning
    // UTC makes every build deterministic regardless of where it runs.
    conn.execute_batch(
        "INSTALL ducklake; LOAD ducklake; INSTALL json; LOAD json; SET TimeZone = 'UTC';",
    )
    .context("loading DuckLake extension")?;

    // Spill configuration (ADR 0005 §1, R7): an incremental bootstrap (or any
    // large source) materializes the whole delta locally before a transform
    // runs. `temp_directory` is unconditional and cheap — it just gives DuckDB
    // somewhere to spill instead of failing when a read exceeds memory.
    // `memory_limit` is opt-in via env because a cgroup-limited pod OOM-kills
    // before DuckDB's host-RAM-derived default would ever trigger that spill;
    // wiring it lets a deploy pin DuckDB's budget under the pod's actual limit.
    let spill_dir = scratch.join("spill");
    std::fs::create_dir_all(&spill_dir)
        .with_context(|| format!("creating spill directory {}", spill_dir.display()))?;
    conn.execute_batch(&format!(
        "SET temp_directory = '{}';",
        esc(&spill_dir.to_string_lossy())
    ))
    .context("setting temp_directory (spill)")?;
    if let Ok(mem) = std::env::var("DATAMK_MEMORY_LIMIT") {
        if !mem.is_empty() {
            conn.execute_batch(&format!("SET memory_limit = '{}';", esc(&mem)))
                .with_context(|| format!("setting memory_limit from DATAMK_MEMORY_LIMIT={mem}"))?;
        }
    }

    let storage = resolve_storage(&b.storage, dir)?;

    let (uses_s3, uses_gcs) = store_secrets_needed(&storage, b);
    if uses_s3 || uses_gcs {
        conn.execute_batch("INSTALL httpfs; LOAD httpfs;")
            .context("loading httpfs extension")?;
    }
    if uses_s3 {
        create_s3_secret(conn, b.s3.as_ref())?;
    }
    if uses_gcs {
        create_gcs_secret(conn, b.gcs.as_ref())?;
    }

    let ro = if read_only { ", READ_ONLY" } else { "" };

    match &b.catalog {
        // Direct-attach mode: a local `.ducklake` file or a self-managed
        // `sqlite:`/`postgres:` DSN — today's behavior, kept for local dev.
        Some(c) => {
            let catalog = resolve_catalog(c, dir)?;
            load_catalog_extension(conn, &catalog)?;
            conn.execute_batch(&format!(
                "ATTACH 'ducklake:{catalog}' AS lake (DATA_PATH '{storage}'{ro}); USE lake;"
            ))
            .with_context(|| {
                format!("attaching DuckLake (catalog={catalog}, storage={storage})")
            })?;
            Ok(None)
        }
        // Published-artifact mode (ADR 0004): the catalog derives from
        // `storage` — fetch the artifact `LATEST` names, attach a private
        // local copy.
        None => {
            if !crate::config::is_remote(&storage) {
                anyhow::bail!(
                    "the profile has no `catalog:` (published-artifact mode), but storage \
                     `{storage}` is not an object store. Published mode derives the catalog \
                     from remote storage (ADR 0004); for local development set `catalog:` \
                     (e.g. ./.cell/catalog.ducklake)."
                );
            }
            let store = Arc::new(Store::for_storage(&storage, b.s3.as_ref(), b.gcs.as_ref())?);
            let data_path = format!("{storage}/data");
            let (local, execution) = match store.latest()? {
                Some(n) => (store.download_execution(n, scratch)?, Some(n)),
                None if read_only => anyhow::bail!(
                    "no published catalog for this cell yet (no catalog/LATEST under \
                     {storage}) — run the Builder (`datamk run`) first"
                ),
                // Bootstrap: the very first execution creates a fresh catalog.
                None => (scratch.join("bootstrap.ducklake"), None),
            };
            // OVERRIDE_DATA_PATH: the artifact records whatever DATA_PATH the
            // host that built it used; the profile's storage is authoritative.
            conn.execute_batch(&format!(
                "ATTACH 'ducklake:{}' AS lake \
                 (DATA_PATH '{}'{ro}, OVERRIDE_DATA_PATH true); USE lake;",
                esc(&local.to_string_lossy()),
                esc(&data_path)
            ))
            .with_context(|| {
                format!("attaching published catalog (execution={execution:?}, storage={storage})")
            })?;
            Ok(Some(PublishedState {
                store,
                local,
                execution,
            }))
        }
    }
}

/// Attach an already-downloaded catalog artifact read-only, bypassing the
/// `LATEST` pointer — the offline-inspection path (`rollback`'s pin guard
/// examines a *target* artifact, not the served one).
pub fn open_artifact(
    local: &Path,
    storage: &str,
    s3: Option<&ResolvedS3>,
    gcs: Option<&ResolvedGcs>,
) -> Result<Connection> {
    let conn = open_duckdb(gcs)?;
    conn.execute_batch(
        "INSTALL ducklake; LOAD ducklake; INSTALL json; LOAD json; \
         INSTALL httpfs; LOAD httpfs; SET TimeZone = 'UTC';",
    )
    .context("loading extensions")?;
    if crate::config::is_gcs(storage) {
        create_gcs_secret(&conn, gcs)?;
    } else {
        create_s3_secret(&conn, s3)?;
    }
    conn.execute_batch(&format!(
        "ATTACH 'ducklake:{}' AS lake \
         (DATA_PATH '{}/data', READ_ONLY, OVERRIDE_DATA_PATH true); USE lake;",
        esc(&local.to_string_lossy()),
        esc(storage)
    ))
    .with_context(|| format!("attaching artifact {}", local.display()))?;
    Ok(conn)
}

/// DuckLake attaches a metadata-DB catalog (`postgres:`/`sqlite:`) through
/// DuckDB's own scanner extension for that database, not something DuckLake
/// bundles itself: without `INSTALL postgres`/`INSTALL sqlite` first, DuckDB
/// doesn't recognize the `postgres://...`/`sqlite:...` DSN as a database at
/// all and instead tries to open it as a literal local file path (the error is
/// exactly that confusing: "Cannot open file
/// .../postgres:/user:pass@host/db: No such file or directory"). A `.ducklake`
/// file catalog needs neither extension, so this only runs for the metadata-DB
/// case.
fn load_catalog_extension(conn: &Connection, catalog: &str) -> Result<()> {
    let ext = if catalog.starts_with("postgres:") {
        "postgres"
    } else if catalog.starts_with("sqlite:") {
        "sqlite"
    } else {
        return Ok(());
    };
    conn.execute_batch(&format!("INSTALL {ext}; LOAD {ext};"))
        .with_context(|| format!("loading DuckDB '{ext}' extension for catalog '{catalog}'"))?;
    Ok(())
}

/// Which store secrets `setup` must register: `(s3, gcs)`. Per scheme in
/// play across storage and sources — a cell can mix `s3://` sources with
/// `gs://` storage and needs both. The block-presence arms cover transforms
/// that read object-store URIs directly (invisible to storage/source
/// inspection): a configured block means the profile expects that store to be
/// reachable. For GCS the block only counts when it carries something DuckDB
/// can use (the HMAC pair or a native extension) — a `credentials:`-only
/// block configures the native store client, not DuckDB.
fn store_secrets_needed(storage: &str, b: &ResolvedBindings) -> (bool, bool) {
    let uses_s3 = crate::config::is_s3(storage)
        || b.sources
            .values()
            .any(|s| s.location().is_some_and(crate::config::is_s3))
        || b.s3.is_some();
    let uses_gcs = crate::config::is_gcs(storage)
        || b.sources
            .values()
            .any(|s| s.location().is_some_and(crate::config::is_gcs))
        || b.gcs
            .as_ref()
            .is_some_and(|g| g.extension.is_some() || (g.key_id.is_some() && g.secret.is_some()));
    (uses_s3, uses_gcs)
}

/// The option list for a DuckDB S3 secret, from the profile's `s3:` block.
/// With explicit key/secret (and optionally a session token) we use static
/// credentials; otherwise DuckDB's `credential_chain` provider resolves AWS
/// env vars, shared profiles, and IAM roles — no secrets in the cell config.
/// Shared between the engine's own secret (below) and `datamk attach`'s
/// printed SQL, so the two can never describe different credentials.
pub(crate) fn s3_secret_options(s3: Option<&ResolvedS3>) -> String {
    let mut parts = vec!["TYPE s3".to_string()];
    let s3 = s3.cloned().unwrap_or(ResolvedS3 {
        region: None,
        endpoint: None,
        url_style: None,
        key_id: None,
        secret: None,
        session_token: None,
        use_ssl: None,
    });

    match (&s3.key_id, &s3.secret) {
        (Some(k), Some(s)) => {
            parts.push(format!("KEY_ID '{}'", esc(k)));
            parts.push(format!("SECRET '{}'", esc(s)));
            // Temporary STS credentials (SSO sessions, assumed roles) are a
            // key triple — without the token the pair alone is invalid.
            if let Some(t) = &s3.session_token {
                parts.push(format!("SESSION_TOKEN '{}'", esc(t)));
            }
        }
        _ => parts.push("PROVIDER credential_chain".to_string()),
    }
    if let Some(r) = &s3.region {
        parts.push(format!("REGION '{}'", esc(r)));
    }
    if let Some(e) = &s3.endpoint {
        parts.push(format!("ENDPOINT '{}'", esc(e)));
    }
    if let Some(u) = &s3.url_style {
        parts.push(format!("URL_STYLE '{}'", esc(u)));
    }
    if let Some(ssl) = s3.use_ssl {
        parts.push(format!("USE_SSL {ssl}"));
    }
    parts.join(", ")
}

/// Register an S3 secret in DuckDB's Secrets Manager (see `s3_secret_options`).
fn create_s3_secret(conn: &Connection, s3: Option<&ResolvedS3>) -> Result<()> {
    conn.execute_batch(&format!(
        "CREATE OR REPLACE SECRET __cell_s3 ({});",
        s3_secret_options(s3)
    ))
    .context("creating S3 secret")?;
    Ok(())
}

/// The `LOAD` statement for a native GCS extension, when the profile names
/// one. Shared between the engine and `datamk attach`'s printed SQL.
pub(crate) fn gcs_load_sql(gcs: Option<&ResolvedGcs>) -> Option<String> {
    gcs.and_then(|g| g.extension.as_ref())
        .map(|p| format!("LOAD '{}';", esc(p)))
}

/// The option list for a DuckDB GCS secret, from the profile's `gcs:` block.
/// Two modes. With `gcs.extension` (a native GCS extension), the secret is
/// `TYPE GCP` and authenticates like the catalog store: the same
/// service-account key (`credentials`) or the ambient ADC chain — no HMAC
/// anywhere. Without it, DuckDB's built-in httpfs reaches GCS through its
/// S3-interoperability API, which only accepts HMAC keys — there is no
/// ADC/credential-chain provider there (httpfs's own `credential_chain` for
/// `TYPE gcs` resolves *AWS* credentials, a footgun we deliberately don't
/// expose), so a missing pair fails loud with both fixes. Shared between the
/// engine's own secret and `datamk attach`'s printed SQL, so the two can
/// never describe different credentials.
pub(crate) fn gcs_secret_options(gcs: Option<&ResolvedGcs>) -> Result<String> {
    if gcs.is_some_and(|g| g.extension.is_some()) {
        let mut parts = vec!["TYPE GCP".to_string()];
        if let Some(c) = gcs.and_then(|g| g.credentials.as_ref()) {
            parts.push(format!("SERVICE_ACCOUNT_KEY_PATH '{}'", esc(c)));
        }
        return Ok(parts.join(", "));
    }
    let (Some(k), Some(s)) = (
        gcs.and_then(|g| g.key_id.as_ref()),
        gcs.and_then(|g| g.secret.as_ref()),
    ) else {
        anyhow::bail!(
            "gs:// is in use but the profile has no `gcs.key_id`/`gcs.secret`. DuckDB's \
             built-in GCS support goes through the S3-interoperability (HMAC) API — create an \
             HMAC key for the bucket (`gcloud storage hmac create`) and set \
             gcs.key_id/gcs.secret in the profile. If your organization forbids HMAC keys, \
             set gcs.extension to a native GCS DuckDB extension binary \
             (northpolesec/duckdb-gcs) and DuckDB will authenticate with ADC instead. \
             (`gcs.credentials`/ADC alone authenticates the catalog store, but not built-in \
             gs:// reads.)"
        );
    };
    let mut parts = vec![
        "TYPE gcs".to_string(),
        format!("KEY_ID '{}'", esc(k)),
        format!("SECRET '{}'", esc(s)),
    ];
    if let Some(e) = gcs.and_then(|g| g.endpoint.as_ref()) {
        parts.push(format!("ENDPOINT '{}'", esc(e)));
    }
    if let Some(ssl) = gcs.and_then(|g| g.use_ssl) {
        parts.push(format!("USE_SSL {ssl}"));
    }
    Ok(parts.join(", "))
}

/// Load the native GCS extension (if configured) and register a GCS secret in
/// DuckDB's Secrets Manager (see `gcs_secret_options`). The extension file is
/// checked first and fails loud — a deployed pod with an absent mount should
/// crash with this error, not limp into an auth failure.
fn create_gcs_secret(conn: &Connection, gcs: Option<&ResolvedGcs>) -> Result<()> {
    if let Some(load) = gcs_load_sql(gcs) {
        let path = gcs.and_then(|g| g.extension.as_deref()).unwrap_or_default();
        if !Path::new(path).is_file() {
            anyhow::bail!(
                "gcs.extension '{path}' does not exist or is not a file — point it at a \
                 gcs.duckdb_extension binary built for this DuckDB version"
            );
        }
        conn.execute_batch(&load)
            .with_context(|| format!("loading native GCS extension {path}"))?;
    }
    conn.execute_batch(&format!(
        "CREATE OR REPLACE SECRET __cell_gcs ({});",
        gcs_secret_options(gcs)?
    ))
    .context("creating GCS secret")?;
    Ok(())
}

/// What `--full-refresh` actually does for this cell (ADR 0005 §3, ADR 0008
/// §6) — three states, not two: re-reading incremental sources takes
/// priority when both are present (its "rewriting watermarks" note already
/// covers the run), a cell with declarative tables but no incremental
/// *sources* still gets a real, from-scratch rebuild (a file-sourced or
/// cell-sourced `materialize:` entry), and only a cell with neither has
/// truly nothing for the flag to do. Pure — unit-testable without a `Cell`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullRefreshEffect {
    IncrementalSources(usize),
    DeclarativeOnly(usize),
    NoEffect,
}

fn full_refresh_effect(incremental_count: usize, declarative_count: usize) -> FullRefreshEffect {
    if incremental_count > 0 {
        FullRefreshEffect::IncrementalSources(incremental_count)
    } else if declarative_count > 0 {
        FullRefreshEffect::DeclarativeOnly(declarative_count)
    } else {
        FullRefreshEffect::NoEffect
    }
}

/// Execute the transform pipeline (the Builder workload): bind sources, run every
/// transform in order inside a single transaction so the result is one atomic
/// DuckLake snapshot, then verify the declared interface against the actual output.
/// In published mode, compact (ADR 0004 §10) and publish the catalog artifact.
/// `retention_secs`: the compaction window; `None` disables compaction.
pub fn run(
    file: &Path,
    profile: &str,
    retention_secs: Option<u64>,
    opts: RunOptions,
) -> Result<()> {
    // Captured before `open()` so the published run summary's `started_at`
    // covers the whole invocation, not just the post-open work.
    let started_at_unix = unix_now();
    let cell = open(file, profile, false)?;
    tracing::info!(cell = %cell.def.cell, profile, "running pipeline");

    // The authoritative conditional-PUT probe (ADR 0004 §3) runs HERE, before
    // any work — in the process that publishes, with the connectivity pods
    // actually have. The deploy host's probe is best-effort (it may not reach
    // an in-cluster or private-endpoint store); this one is the guarantee,
    // and via the init Job it fails the deploy with the build pod's logs.
    if let Some(p) = &cell.published {
        match p.store.probe_conditional_put() {
            Ok(crate::store::ProbeOutcome::Enforced) => {}
            Ok(crate::store::ProbeOutcome::NotEnforced) => {
                anyhow::bail!(crate::store::NOT_ENFORCED_MSG)
            }
            Err(e) => {
                return Err(e.context("probing the object store before building (ADR 0004 §3)"))
            }
        }
    }

    let incremental_count = cell
        .sources
        .values()
        .filter(|s| {
            matches!(
                s,
                ResolvedSource::Connection {
                    incremental: Some(_),
                    ..
                }
            )
        })
        .count();

    // ADR 0008: --full-refresh also rebuilds every table from scratch (a
    // real effect, and the schema-migration/hard-delete-reconciliation
    // path), even when the cell has no incremental *sources* at all. The
    // stale "no effect" warning would lie in that shape. Every transform is
    // declarative now (one language — no raw entries left to exclude), so
    // this is just the transform count.
    let declarative_count = cell.transforms.len();

    // ADR 0005 §3: the most expensive flag this engine has must never be
    // silent. Announce before binding, and warn (rather than silently
    // no-op) only when the flag truly has nothing to do.
    if opts.full_refresh {
        match full_refresh_effect(incremental_count, declarative_count) {
            FullRefreshEffect::IncrementalSources(n) => tracing::info!(
                "full refresh: re-reading {n} incremental sources from zero, rewriting watermarks"
            ),
            FullRefreshEffect::DeclarativeOnly(n) => tracing::info!(
                "full refresh: rebuilding {n} declarative table(s) from scratch; no incremental \
                 watermarks to rewind"
            ),
            FullRefreshEffect::NoEffect => tracing::warn!(
                "--full-refresh has no effect: this cell declares no incremental sources"
            ),
        }
    }
    if opts.verify_replay && incremental_count == 0 {
        tracing::warn!("--verify-replay has no effect: this cell declares no incremental sources");
    }

    // Sources are session-local TEMP VIEWs: visible to transforms, never committed
    // to the catalog.
    connectors::prepare(&cell.sources, &cell.dir)?;
    // View-backed connection sources (BigQuery views/materialized
    // views/external tables): classification is batched at most once per
    // (connection, dataset) for this whole run, built up front from every
    // declared source.
    let mut classify_cache = ClassifyCache::new(&cell.sources);
    let mut advances: Vec<WatermarkAdvance> = Vec::new();
    // One entry per declared source regardless of kind — the published run
    // summary's `sources` array (design doc: persistent run logs + a
    // published run-summary). Collected unconditionally; only used when
    // `cell.published` is `Some` below (direct-attach mode skips writing
    // the summary entirely — no `catalog/` prefix exists to write it under).
    let mut source_infos: Vec<SourceRunInfo> = Vec::new();
    for (i, (name, src)) in cell.sources.iter().enumerate() {
        let outcome = bind_source(
            &cell.conn,
            i,
            name,
            src,
            &cell.dir,
            cell.s3.as_ref(),
            cell.gcs.as_ref(),
            &cell.scratch,
            opts.full_refresh,
            &mut classify_cache,
            &cell.def.cell,
        )?;
        if let Some(adv) = outcome.advance {
            advances.push(adv);
        }
        source_infos.push(outcome.info);
    }

    // The shrink detector (ADR 0005 §2 item 2): truncation (`CREATE OR REPLACE
    // ... FROM <incremental view>`) is idempotent and invisible to
    // --verify-replay, so it is caught here instead — a warning, not a gate,
    // since legitimate rebuilds also shrink tables. Recorded before BEGIN so
    // "before" reflects the artifact's state walking into this execution.
    let shrink_before = if incremental_count > 0 {
        Some(snapshot_table_counts(&cell.conn)?)
    } else {
        None
    };

    cell.conn
        .execute_batch("BEGIN")
        .context("begin transaction")?;
    // Per-transform timing for the published run summary — cheap
    // (`Instant`, no extra queries) and the one piece of this design that
    // can't be reconstructed from anything else `run` already narrates.
    let mut transform_infos: Vec<TransformRunInfo> = Vec::new();
    for t in &cell.transforms {
        let started = Instant::now();
        execute_transform(&cell.conn, &cell.dir, t, opts.full_refresh)?;
        transform_infos.push(TransformRunInfo {
            file: t.file_path().to_string(),
            duration_ms: started.elapsed().as_millis() as u64,
        });
    }
    // Watermarks persist inside the same transaction as the data they
    // account for (ADR 0005 §1): atomic with the snapshot, before COMMIT.
    persist_watermarks(&cell.conn, &advances, opts.full_refresh)?;
    cell.conn
        .execute_batch("COMMIT")
        .context("commit snapshot")?;
    // Cheap and best-effort, and must run before DETACH below (published
    // branch) — `lake` needs to still be attached to answer this.
    let snapshot_id = current_snapshot_id(&cell.conn);

    // Collected here (still inside the function, before compact()/DETACH
    // may run below) but printed at the very end of `run` — see the
    // eprintln! block near the bottom — so the finding is the last thing an
    // operator sees, not a tracing line scrolled past by later narration.
    let mut shrunk: Vec<(String, i64, i64)> = Vec::new();
    if let Some(before) = &shrink_before {
        let after = snapshot_table_counts(&cell.conn)?;
        shrunk = shrunk_tables(before, &after);
        for (table, before_count, after_count) in &shrunk {
            tracing::warn!(
                table = %table, before = before_count, after = after_count,
                "table shrank during a run with incremental sources — likely cause: a \
                 `CREATE OR REPLACE` rebuilt this table from an incremental source view \
                 instead of merging into it (see docs/guides/incremental.md)"
            );
        }
    }

    tracing::info!("verifying interface");
    // Verify gates publish (ADR 0004 §4): a failed contract check must never
    // enter published history.
    crate::verify::check(&cell.conn, &cell.def)?;

    // --verify-replay (ADR 0005 §2 item 1): after COMMIT and after
    // verify::check, before compact()/DETACH — pinned ordering. It needs the
    // committed snapshot to diff against and must run before the artifact is
    // handed to compaction/publish.
    if incremental_count > 0 && opts.verify_replay {
        verify_replay(&cell, opts.full_refresh)?;
    }

    if let Some(p) = &cell.published {
        // Compaction (ADR 0004 §10) runs before publish so the artifact ships
        // already-compacted. Best-effort: a maintenance failure must not turn
        // a good build into a failed execution.
        if let Some(secs) = retention_secs {
            if let Err(e) = compact(&cell, secs) {
                tracing::warn!(error = %e, "compaction failed; publishing uncompacted");
            }
        }

        // Detach cleanly so the artifact is quiescent before upload (§4);
        // this run is the only writer (§5).
        cell.conn
            .execute_batch("USE memory; DETACH lake;")
            .context("detaching catalog before publish")?;
        let n = p
            .store
            .publish_execution(&p.local)
            .context("publishing catalog artifact")?;
        tracing::info!(execution = n, "published catalog artifact");

        // The published run summary (design: persistent run logs + a
        // published run-summary): denormalized narration alongside the
        // artifact, never truth — a write failure (including a same-N
        // collision, which execution numbering should make impossible) is
        // a warning, never a run failure.
        let summary = RunSummary {
            execution: n,
            snapshot_id,
            started_at: rfc3339_utc(started_at_unix),
            finished_at: rfc3339_utc(unix_now()),
            datamk_version: env!("CARGO_PKG_VERSION").to_string(),
            verify_outcome: "passed".to_string(),
            sources: source_infos,
            transforms: transform_infos,
        };
        if let Err(e) = write_run_summary(&p.store, &summary) {
            tracing::warn!(
                execution = n, error = %e,
                "failed to write the run summary — the published artifact itself is unaffected"
            );
        }

        // Superseded-artifact GC (§10), after publish so `keep` is the number
        // LATEST now names. Best-effort for the same reason as above.
        if let Some(secs) = retention_secs {
            let cutoff = unix_now() - secs as i64;
            match p.store.gc_artifacts(cutoff, &[n]) {
                Ok(deleted) if !deleted.is_empty() => {
                    tracing::info!(count = deleted.len(), "garbage-collected old artifacts");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "artifact GC failed; artifacts retained"),
            }
        }
    }

    tracing::info!("pipeline complete");

    // The shrink detector's summary (ADR 0005 §2 item 2), promoted to the
    // literal last thing printed: `tracing::warn!` above already narrated
    // each shrunk table as it was found, but a mid-run log line is easy to
    // scroll past on a noisy build. Printed after "pipeline complete" (not
    // before) so nothing — including that line — follows it. This block is
    // `eprintln!`, not `tracing`, so it survives an aggressive `RUST_LOG`
    // filter too. Advisory only — exit code stays 0 (a legitimate full
    // rebuild also shrinks a table); this is a summary, not a gate.
    if !shrunk.is_empty() {
        for line in format_shrink_summary_lines(&shrunk) {
            eprintln!("{line}");
        }
    }

    Ok(())
}

// --- ADR 0008: declarative incremental materialization ---------------------
//
// A `transforms:` entry dispatches through `execute_transform`: the raw
// shape executes its file verbatim, exactly as before; the declarative
// shape has the engine compose and run the stage/guard/strategy DML
// sequence below. Shared by `run` and `verify_replay` so a replay exercises
// the identical composed statements a real run would (§5b) — a declarative
// entry's replay-safety is tested for real, not assumed by construction and
// left unverified.

/// Double-quote an identifier for the SQL build site, escaping any embedded
/// `"`. Every identifier reaching here (`key:`, a resolved table name) is
/// already shape-validated at resolve time (`config::resolve_transforms`) —
/// this is defense in depth, the primary control (ADR 0008 §7).
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Execute one transform entry inside the caller's transaction.
fn execute_transform(
    conn: &Connection,
    dir: &Path,
    entry: &ResolvedTransform,
    full_refresh: bool,
) -> Result<()> {
    // `replace` (ADR 0008 decision 3) is a different shape entirely — one
    // statement, no staging, no key, no guards — so it dispatches to its
    // own function rather than threading a "strategy that skips
    // everything" branch through `execute_materialize`.
    match entry.strategy {
        MaterializeStrategy::Replace => execute_replace(conn, dir, &entry.sql, &entry.table),
        MaterializeStrategy::Upsert | MaterializeStrategy::Append => execute_materialize(
            conn,
            dir,
            &entry.sql,
            entry.strategy,
            &entry.key,
            &entry.table,
            full_refresh,
        ),
    }
}

/// The composition-failure error context shared by every place a
/// transform's file text is wrapped as a subquery (`execute_replace`'s
/// single statement, `execute_materialize`'s staging statement). ADR 0008:
/// "There is one language for transforms... every transform is a
/// SELECT-only file. No transform contains DDL or DML." A file carrying
/// hand-written DDL/DML — or any extra statement — fails to prepare here,
/// loudly, because it no longer parses as a single subquery; this names the
/// rule rather than leaving the author with only DuckDB's raw parser error.
/// The trailing-semicolon hint is a pure string check on the file text, not
/// SQL parsing — the single most common way to hit this — appended only
/// when the text actually ends with `;`, never implied for an unrelated
/// parse error.
fn composition_error_context(action: &str, select_text: &str) -> String {
    let mut ctx = format!(
        "{action}: transform files contain exactly one SELECT; the engine owns all \
         CREATE/MERGE/INSERT (ADR 0008). If this file carries hand-written DDL, keep its \
         SELECT and pick a materialize: strategy."
    );
    if select_text.trim_end().ends_with(';') {
        ctx.push_str(
            " Remove the trailing semicolon: the file is wrapped as a subquery \
             (docs/guides/incremental.md §4).",
        );
    }
    ctx
}

/// `materialize: replace` (ADR 0008 decision 3): rebuild the table from
/// scratch every run, one statement, `CREATE OR REPLACE TABLE "<table>" AS
/// (<file text>)`. No staging relation — there is nothing to evaluate
/// twice against (no key, no guard needs a stable relation to query). No
/// NULL-key/duplicate-key guards — both guard a `key:` that `replace`
/// doesn't have. No schema-drift check — `replace` recreates the table at
/// the SELECT's current shape every run by design, so "drift" is not an
/// error state here, it's the mechanism. Legal only where guard 4c
/// (`verify::check_replace_incremental_gate`, `config::load`) allows —
/// enforced once, at resolve time, before this ever runs, not re-checked
/// here.
fn execute_replace(conn: &Connection, dir: &Path, sql_path: &str, table: &str) -> Result<()> {
    let table_q = quote_ident(table);
    let select_path = dir.join(sql_path);
    let select_text = std::fs::read_to_string(&select_path)
        .with_context(|| format!("reading transform {}", select_path.display()))?;
    let stmt = format!("CREATE OR REPLACE TABLE {table_q} AS ({select_text});");
    tracing::info!(table = %table, sql = %stmt, "materialize: replace");
    conn.execute_batch(&stmt).with_context(|| {
        composition_error_context(
            &format!("replacing table '{table}' from '{sql_path}'"),
            &select_text,
        )
    })?;

    let artifact = write_eject_artifact(dir, table, &[], &[stmt])?;
    log_eject_notice(table, &artifact);
    Ok(())
}

/// The engine-composed sequence for one `materialize:` entry (ADR 0008 §4):
/// stage the SELECT once, bootstrap, guard, then either the strategy DML or
/// (on `--full-refresh`) a from-scratch rebuild. Every generated statement
/// is logged verbatim at `tracing::info!` and written to the eject artifact
/// (§7) — plain, portable DuckDB an author can inspect or run by hand
/// against the lake, with zero data movement.
fn execute_materialize(
    conn: &Connection,
    dir: &Path,
    sql_path: &str,
    strategy: MaterializeStrategy,
    key: &[String],
    table: &str,
    full_refresh: bool,
) -> Result<()> {
    let staging = format!("__datamk_stage_{table}");
    let staging_q = quote_ident(&staging);
    let table_q = quote_ident(table);

    // (a) Stage the SELECT exactly once. MUST be TEMP — gate 3 (ADR 0008
    // Open verification gates §3) proved that after `USE lake`, a non-TEMP
    // staged relation does not resolve unqualified. MUST be `CREATE OR
    // REPLACE` (ADR 0008 §7, amended after the first eject gate-2 proof
    // attempt failed here): an eject artifact is only a real eject artifact
    // if the pasted file survives being executed twice in one connection —
    // exactly what `--verify-replay` does to every entry's staged statement.
    // A bare `CREATE TEMP TABLE` errors "already exists" on the second run;
    // `OR REPLACE` is what makes the logged DML paste-safe as well as
    // engine-safe.
    let select_path = dir.join(sql_path);
    let select_text = std::fs::read_to_string(&select_path)
        .with_context(|| format!("reading transform {}", select_path.display()))?;
    let stage_stmt = format!("CREATE OR REPLACE TEMP TABLE {staging_q} AS ({select_text});");
    tracing::info!(table = %table, sql = %stage_stmt, "materialize: staging");
    conn.execute_batch(&stage_stmt).with_context(|| {
        composition_error_context(
            &format!("staging '{sql_path}' into {staging}"),
            &select_text,
        )
    })?;

    // (h) The staging relation is engine-owned scratch — dropped on every
    // exit path, including an early return through `?` in the closure below
    // (a guard, drift, or strategy failure), so a failed entry never leaves
    // it behind for the next `datamk run` in the same session
    // (`verify_replay` re-executes every entry in the same connection). Not
    // logged as part of the eject artifact (ADR 0008 §7): paste-safety no
    // longer depends on the DROP now that staging is `CREATE OR REPLACE` —
    // this is engine hygiene only, invisible to an author who pastes the
    // three logged statements.
    let result = (|| -> Result<()> {
        // (b) Bootstrap: proven to short-circuit rather than evaluate the
        // full SELECT (gate 3 — see the sub-10ms canary test).
        let bootstrap_stmt =
            format!("CREATE TABLE IF NOT EXISTS {table_q} AS SELECT * FROM {staging_q} LIMIT 0;");
        tracing::info!(table = %table, sql = %bootstrap_stmt, "materialize: bootstrap");
        conn.execute_batch(&bootstrap_stmt)
            .with_context(|| format!("bootstrapping table '{table}'"))?;

        // (c) Guards, before any strategy DML. §5c: the engine owns the
        // loudness, not the database — a gate test disproved the assumption
        // that `MERGE` itself errors on a duplicate-key delta (see the
        // pinned `merge_surprise_*` tests). Applies to both strategies:
        // `append`'s anti-join only excludes keys already IN the target, so
        // two NEW rows sharing a key would both insert unless caught here
        // too.
        check_null_keys(conn, sql_path, &staging_q, key)?;
        check_duplicate_keys(conn, sql_path, &staging_q, key)?;

        let staging_cols = describe_shape(conn, &staging_q)?;
        let table_cols = describe_shape(conn, &table_q)?;

        if full_refresh {
            // (f) Rebuilds from scratch — the one place `CREATE OR REPLACE`
            // is correct, because the engine issues it from a full re-read,
            // never a delta. Schema drift is moot (that's the rebuild's
            // whole purpose); the guards above still apply — a NULL or
            // duplicate key must not survive into a freshly rebuilt table
            // either.
            let stmt = format!("CREATE OR REPLACE TABLE {table_q} AS SELECT * FROM {staging_q};");
            tracing::info!(table = %table, sql = %stmt, "materialize: full-refresh rebuild");
            conn.execute_batch(&stmt)
                .with_context(|| format!("rebuilding table '{table}' (--full-refresh)"))?;
            let artifact = write_eject_artifact(
                dir,
                table,
                key,
                &[stage_stmt.clone(), bootstrap_stmt.clone(), stmt],
            )?;
            log_eject_notice(table, &artifact);
            return Ok(());
        }

        // (d) Schema drift, checked before any strategy DML so the error is
        // ours — named and actionable — not a raw DuckDB binder error.
        if let Some(drift) = schema_drift(&staging_cols, &table_cols) {
            anyhow::bail!(
                "declarative table '{table}': {drift}. Declarative materialization does not \
                 migrate schema in place, and there is no ALTER path inside the pipeline (ADR \
                 0008 §6). Recover with `datamk run --full-refresh` to rebuild the table at the \
                 new shape, or use `datamk attach` for one-off out-of-pipeline surgery."
            );
        }

        // (e) Strategy DML. Non-key columns are read from the introspected
        // table schema (`table_cols`, from `DESCRIBE`), never from parsing
        // the SELECT (ADR 0008 §4: "reading a relation's column names is
        // metadata, not query comprehension").
        let non_key_cols: Vec<(String, String)> = table_cols
            .iter()
            .filter(|(c, _)| !key.iter().any(|k| k.eq_ignore_ascii_case(c)))
            .cloned()
            .collect();
        let stmt = match strategy {
            MaterializeStrategy::Upsert => build_merge(&table_q, &staging_q, key, &non_key_cols),
            MaterializeStrategy::Append => {
                build_anti_join_insert(&table_q, &staging_q, key, &table_cols)
            }
            MaterializeStrategy::Replace => unreachable!(
                "execute_transform routes `replace` to execute_replace, never here — replace \
                 takes no key and needs none of this function's staging/guards"
            ),
        };
        tracing::info!(table = %table, strategy = %strategy, sql = %stmt, "materialize: strategy");
        conn.execute_batch(&stmt)
            .with_context(|| format!("applying `{strategy}` to table '{table}'"))?;
        let artifact =
            write_eject_artifact(dir, table, key, &[stage_stmt.clone(), bootstrap_stmt, stmt])?;
        log_eject_notice(table, &artifact);
        Ok(())
    })();

    let drop_stmt = format!("DROP TABLE {staging_q};");
    if let Err(e) = conn.execute_batch(&drop_stmt) {
        tracing::warn!(table = %table, staging = %staging, error = %e, "failed to drop staging relation");
    }

    result
}

/// ADR 0008 §7 (amended after the eject gate-2 re-proof): verbatim DML in a
/// log line proved un-greppable in practice. The eject artifact is now a
/// **written file**, `.cell/materialize/<table>.sql` — the exact statements
/// this run executed for `table`, headed by a comment naming what the file
/// does *not* carry (the engine-side guards) and the explicit-`grain:`
/// requirement. Generated state alongside the rest of `.cell/` (the catalog
/// file, the release manifest), resolved the same way — against `dir`, the
/// directory containing `cell.yaml` — and overwritten every run; not part
/// of the contract, not meant to be hand-edited in place (edit the `sql:`
/// file and let the next run regenerate this).
///
/// `statements` is staging + bootstrap + (strategy or the full-refresh
/// rebuild) — literally what ran this run, in that order, so the file is
/// always paste-runnable exactly as written, no editing required.
///
/// `key` doubles as the strategy discriminant for the header text: empty
/// only for `replace` (schema-validated — `append`/`upsert` always carry a
/// non-empty `key:`), which never ran any guard to lose, so its header says
/// that plainly instead of naming guards that don't apply to it.
fn write_eject_artifact(
    dir: &Path,
    table: &str,
    key: &[String],
    statements: &[String],
) -> Result<PathBuf> {
    let materialize_dir = dir.join(".cell").join("materialize");
    std::fs::create_dir_all(&materialize_dir)
        .with_context(|| format!("creating {}", materialize_dir.display()))?;
    let path = materialize_dir.join(format!("{table}.sql"));

    let header = if key.is_empty() {
        format!(
            "-- {table}.sql — generated by `datamk run` (ADR 0008 §7). Overwritten every run;\n\
             -- not part of the contract; not meant to be hand-edited in place.\n\
             --\n\
             -- (a) This file is the exact DML the engine executed THIS RUN for table \"{table}\".\n\
             -- (b) `replace` rebuilds this table from scratch every run — no `key:`, and no\n\
             --     NULL-key/duplicate-key/schema-drift guards ever ran (they guard a key this\n\
             --     strategy doesn't have). There is nothing this file could lose by being\n\
             --     pasted; it is already the whole story.\n\
             -- (c) This is an audit/portability artifact, not a migration path (ADR 0008 §7):\n\
             --     plain DuckDB, runnable anywhere, so the abstraction is inspectable and the\n\
             --     data layer never depends on datamk to be read. There is no supported way to\n\
             --     point cell.yaml `transforms:` at this file directly — edit the `sql:` file's\n\
             --     SELECT and let the next run regenerate this.\n\
             -- (d) Full recipe: docs/guides/incremental.md §4.\n\n"
        )
    } else {
        format!(
            "-- {table}.sql — generated by `datamk run` (ADR 0008 §7). Overwritten every run;\n\
             -- not part of the contract; not meant to be hand-edited in place.\n\
             --\n\
             -- (a) This file is the exact DML the engine executed THIS RUN for table \"{table}\".\n\
             -- (b) The NULL-key, duplicate-key, and schema-drift guards are engine-side checks —\n\
             --     they are NOT in this file. A pasted copy of the statements below does not\n\
             --     carry them.\n\
             -- (c) This is an audit/portability artifact, not a migration path (ADR 0008 §7):\n\
             --     plain DuckDB, runnable anywhere, so the abstraction is inspectable and the\n\
             --     data layer never depends on datamk to be read. There is no supported way to\n\
             --     point cell.yaml `transforms:` at this file directly — edit the `sql:` file's\n\
             --     SELECT and let the next run regenerate this.\n\
             -- (d) Full recipe: docs/guides/incremental.md §4.\n\n"
        )
    };

    let mut body = header;
    for stmt in statements {
        body.push_str(stmt);
        body.push('\n');
    }

    std::fs::write(&path, &body)
        .with_context(|| format!("writing eject artifact {}", path.display()))?;
    Ok(path)
}

/// The one-line pointer logged after the artifact is written — the audit
/// trail (the three `tracing::info!("materialize: ...")` DML lines above
/// this in the run log) stays; this replaces the old ~90-word inline eject
/// paragraph now that the guard-trade explanation lives in the artifact's
/// own header comment.
fn log_eject_notice(table: &str, artifact_path: &Path) {
    tracing::info!(
        table = %table,
        "materialize: eject artifact {} (engine guards not included — see \
         docs/guides/incremental.md §4)",
        artifact_path.display()
    );
}

/// §7: NULL in any key column accumulates unboundedly under the equality/
/// `IN` semantics anti-join and `MERGE` both use (`NULL` never matches
/// `NULL`). Hard error naming the column and the staged-delta NULL count,
/// before any strategy DML — the exact wording ADR 0008 §7 specifies.
fn check_null_keys(
    conn: &Connection,
    sql_path: &str,
    staging_q: &str,
    key: &[String],
) -> Result<()> {
    for k in key {
        let kq = quote_ident(k);
        let null_count: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM {staging_q} WHERE {kq} IS NULL"),
                [],
                |r| r.get(0),
            )
            .with_context(|| format!("checking key column '{k}' for NULLs"))?;
        if null_count > 0 {
            anyhow::bail!(
                "transform '{sql_path}': materialize key column '{k}' contains NULL in the \
                 staged delta ({null_count} rows). Rows with a NULL key cannot be deduplicated \
                 and would accumulate a new copy every run. Make '{k}' NOT NULL upstream, or \
                 filter NULLs out in the SELECT."
            );
        }
    }
    Ok(())
}

/// §5c (corrected, engine-owned): a delta carrying two rows for one key
/// makes `MERGE`/anti-join violate the one-row-per-key invariant silently —
/// a gate test disproved the earlier assumption that the database itself
/// errors on this. `GROUP BY key HAVING count(*) > 1` on the staged
/// relation, before any strategy DML, naming the offending key values and
/// the `QUALIFY` fix.
fn check_duplicate_keys(
    conn: &Connection,
    sql_path: &str,
    staging_q: &str,
    key: &[String],
) -> Result<()> {
    let group_cols = key
        .iter()
        .map(|k| quote_ident(k))
        .collect::<Vec<_>>()
        .join(", ");
    let display_expr = format!(
        "concat_ws(' | ', {})",
        key.iter()
            .map(|k| format!("CAST({} AS VARCHAR)", quote_ident(k)))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let mut stmt = conn
        .prepare(&format!(
            "SELECT {display_expr} AS key_display, count(*) AS n FROM {staging_q} \
             GROUP BY {group_cols} HAVING count(*) > 1 ORDER BY n DESC LIMIT 5"
        ))
        .context("preparing duplicate-key check")?;
    let offenders: Vec<(String, i64)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .context("checking staged delta for duplicate keys")?
        .collect::<std::result::Result<_, _>>()?;
    if offenders.is_empty() {
        return Ok(());
    }

    let total_offending: i64 = conn.query_row(
        &format!(
            "SELECT count(*) FROM (SELECT {group_cols} FROM {staging_q} GROUP BY {group_cols} \
             HAVING count(*) > 1)"
        ),
        [],
        |r| r.get(0),
    )?;
    let sample = offenders
        .iter()
        .map(|(v, n)| format!("{v} ({n}x)"))
        .collect::<Vec<_>>()
        .join(", ");
    let key_csv = key
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!(
        "transform '{sql_path}': materialize key {key:?} is not unique in the staged delta — \
         {total_offending} key value(s) appear more than once (e.g. {sample}). Declarative \
         materialization requires one row per key in the delta; dedupe in your SELECT with \
         `QUALIFY row_number() OVER (PARTITION BY {key_csv} ORDER BY <col>) = 1`, naming the \
         column that should decide which row wins."
    );
}

/// `(name, type)` for every column of a relation, in declared order —
/// `describe_columns` (above, ADR 0005 §1) already does exactly this via
/// `DESCRIBE SELECT * FROM <qualified>`, plus nullability this call site
/// doesn't need. Used for schema-drift comparison and to introspect the
/// target table's column list for the strategy DML's projection.
fn describe_shape(conn: &Connection, quoted_name: &str) -> Result<Vec<(String, String)>> {
    Ok(describe_columns(conn, quoted_name)?
        .into_iter()
        .map(|(name, ty, _nullable)| (name, ty))
        .collect())
}

/// Pure comparator (unit-testable without a connection): the first drift
/// between a staged delta's shape and the accumulated table's shape, or
/// `None`. Checks staging-not-in-table (new column / type change) in
/// staging order first, then table-not-in-staging (dropped column) in table
/// order — deterministic, and always names one drift, not every drift at
/// once (ADR 0008 §8: one mode, fail, name it).
fn schema_drift(staging: &[(String, String)], table: &[(String, String)]) -> Option<String> {
    for (name, ty) in staging {
        match table.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
            None => {
                return Some(format!(
                    "the SELECT now yields column '{name}' ({ty}) absent from the accumulated \
                     table"
                ))
            }
            Some((_, table_ty)) if !table_ty.eq_ignore_ascii_case(ty) => {
                return Some(format!(
                    "column '{name}' changed type from {table_ty} (accumulated table) to {ty} \
                     (the SELECT)"
                ))
            }
            _ => {}
        }
    }
    for (name, _) in table {
        if !staging.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
            return Some(format!(
                "the accumulated table has column '{name}' that the SELECT no longer yields"
            ));
        }
    }
    None
}

/// `upsert`'s primitive (ADR 0008 §5, proven): `MERGE INTO <table> USING
/// <staging> AS s ON <key equality> WHEN MATCHED THEN UPDATE SET
/// <non-key...> WHEN NOT MATCHED THEN INSERT (<all cols>) VALUES (<s.*>)`.
/// The target is referenced by its quoted name unaliased, matching the
/// exact shape the gate tests proved (`merge_in_transaction_against_attached_ducklake_table`
/// et al.). `WHEN MATCHED` is omitted entirely when there are no non-key
/// columns — matched rows are identical by the ON condition, nothing to
/// update, and an empty `UPDATE SET` is not valid SQL.
fn build_merge(
    table_q: &str,
    staging_q: &str,
    key: &[String],
    non_key_cols: &[(String, String)],
) -> String {
    let on = key
        .iter()
        .map(|k| {
            let kq = quote_ident(k);
            format!("{table_q}.{kq} = s.{kq}")
        })
        .collect::<Vec<_>>()
        .join(" AND ");

    let insert_cols: Vec<String> = key
        .iter()
        .map(|k| quote_ident(k))
        .chain(non_key_cols.iter().map(|(c, _)| quote_ident(c)))
        .collect();
    let insert_values: Vec<String> = key
        .iter()
        .map(|k| format!("s.{}", quote_ident(k)))
        .chain(
            non_key_cols
                .iter()
                .map(|(c, _)| format!("s.{}", quote_ident(c))),
        )
        .collect();

    let when_matched = if non_key_cols.is_empty() {
        String::new()
    } else {
        let set_list = non_key_cols
            .iter()
            .map(|(c, _)| {
                let cq = quote_ident(c);
                format!("{cq} = s.{cq}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(" WHEN MATCHED THEN UPDATE SET {set_list}")
    };

    format!(
        "MERGE INTO {table_q} USING {staging_q} AS s ON {on}{when_matched} WHEN NOT MATCHED \
         THEN INSERT ({}) VALUES ({});",
        insert_cols.join(", "),
        insert_values.join(", "),
    )
}

/// `append`'s primitive (ADR 0008 §4): an anti-join on the key, inserting
/// only delta rows whose key is not already present. An explicit column
/// list on both sides (rather than the ADR §4 sketch's bare `SELECT s.*`) —
/// a deliberate hardening: `s.*`'s correctness would depend on staging and
/// the target table sharing not just the same columns but the same
/// *positional order*, which `schema_drift`'s name-based comparison does
/// not guarantee. Column names, introspected from the target table, remove
/// that assumption entirely.
fn build_anti_join_insert(
    table_q: &str,
    staging_q: &str,
    key: &[String],
    table_cols: &[(String, String)],
) -> String {
    let cols: Vec<String> = table_cols.iter().map(|(c, _)| quote_ident(c)).collect();
    let select_cols: Vec<String> = table_cols
        .iter()
        .map(|(c, _)| format!("s.{}", quote_ident(c)))
        .collect();
    let on = key
        .iter()
        .map(|k| {
            let kq = quote_ident(k);
            format!("d.{kq} = s.{kq}")
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    format!(
        "INSERT INTO {table_q} ({}) SELECT {} FROM {staging_q} s ANTI JOIN {table_q} d ON {on};",
        cols.join(", "),
        select_cols.join(", "),
    )
}

/// Alias kept local to this module (most call sites here predate
/// `timeutil` and just want a cutoff to subtract from) — same
/// implementation as `crate::timeutil::unix_now`.
fn unix_now() -> i64 {
    crate::timeutil::unix_now()
}

/// The current max snapshot id, if cheaply queryable — best-effort, for the
/// published run summary only. Must be called while `lake` is still
/// attached (before the publish branch's `DETACH`). `None` on any failure
/// (an unexpected catalog shape, e.g.) rather than propagating: this is
/// narration, and a run that otherwise succeeded must not fail over it.
fn current_snapshot_id(conn: &Connection) -> Option<i64> {
    conn.query_row(
        "SELECT max(snapshot_id) FROM ducklake_snapshots('lake')",
        [],
        |r| r.get(0),
    )
    .ok()
}

/// Write the run summary alongside the catalog artifact just published
/// (`catalog/executions/<N>.run.json`) — conditionally, matching the
/// artifact's own immutability model (`put_if_absent`, the same primitive
/// `publish_execution` uses for the `.ducklake` file itself, not the plain
/// `put` `LATEST` uses — a run summary is per-execution and immutable, not
/// a mutable pointer). `Ok(false)` from `put_if_absent` (the key already
/// existed) is folded into an `Err` here so the caller's uniform
/// warn-and-continue covers it too; execution numbers are monotonic, so
/// this should never actually happen.
fn write_run_summary(store: &Store, summary: &RunSummary) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(summary).context("serializing run summary")?;
    let key = crate::store::run_summary_key(summary.execution);
    let created = store
        .put_if_absent(&key, bytes)
        .with_context(|| format!("writing run summary {key}"))?;
    if !created {
        anyhow::bail!("run summary key {key} already existed (execution numbers are monotonic)");
    }
    Ok(())
}

/// Compaction inside the working catalog (ADR 0004 §10), before publish:
///
/// 1. **Expire snapshots** older than the retention window — by explicit id
///    list, so a pinned snapshot (the release manifest ships into the pod
///    with the cell content) and the newest snapshot are never expired.
/// 2. **Delete unreferenced data files** whose expiry is at least one full
///    retention window old. The lag is what keeps *old artifacts* consistent:
///    any artifact old enough to still reference such a file has itself been
///    GC'd (same window) before the file goes away.
fn compact(cell: &Cell, retention_secs: u64) -> Result<()> {
    let cutoff = unix_now() - retention_secs as i64;
    let pins = crate::manifest::Published::load(&cell.dir)
        .map(|p| p.pinned_snapshots())
        .unwrap_or_default();

    let mut stmt = cell
        .conn
        .prepare(
            "SELECT snapshot_id, epoch(snapshot_time)::BIGINT \
             FROM ducklake_snapshots('lake')",
        )
        .context("querying snapshots for compaction")?;
    let snapshots: Vec<(i64, i64)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<std::result::Result<_, _>>()?;

    let expire = select_expirable(&snapshots, &pins, cutoff);
    if !expire.is_empty() {
        let ids = expire
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        cell.conn
            .execute_batch(&format!(
                "CALL ducklake_expire_snapshots('lake', versions => [{ids}]);"
            ))
            .context("expiring snapshots")?;
        tracing::info!(count = expire.len(), "expired snapshots past retention");
    }

    cell.conn
        .execute_batch(&format!(
            "CALL ducklake_cleanup_old_files('lake', older_than => to_timestamp({cutoff}));"
        ))
        .context("cleaning up unreferenced data files")?;
    Ok(())
}

/// Which snapshot ids to expire: strictly older than the cutoff, never a
/// pinned id, never the newest snapshot (the artifact's current state).
fn select_expirable(snapshots: &[(i64, i64)], pins: &[i64], cutoff_unix: i64) -> Vec<i64> {
    let newest = snapshots.iter().map(|(id, _)| *id).max();
    snapshots
        .iter()
        .filter(|(id, time)| *time < cutoff_unix && !pins.contains(id) && Some(*id) != newest)
        .map(|(id, _)| *id)
        .collect()
}

/// What binding one source produced: its watermark advance (incremental
/// connection sources only — `None` for every other shape) and its
/// contribution to the published run summary (ADR/design: persistent run
/// logs + a published run-summary). Two different consumers of the same
/// bind pass, threaded as one struct rather than a second engine-wide
/// collection pass.
struct BindOutcome {
    advance: Option<WatermarkAdvance>,
    info: SourceRunInfo,
}

/// Bind one source as a TEMP VIEW. Raw sources read a path directly; cell sources
/// attach another cell's DuckLake read-only and read its table by name (optionally
/// pinned to a snapshot) — composing through the catalog, not raw files. Returns
/// the source's watermark advance when it is an incremental connection source
/// (`None` for every other arm) — `run` threads the collected advances across
/// the pre-BEGIN/inside-the-transaction boundary (ADR 0005 §1) — plus a
/// `SourceRunInfo` for the published run summary, for every arm.
#[allow(clippy::too_many_arguments)]
fn bind_source(
    conn: &Connection,
    idx: usize,
    name: &str,
    src: &ResolvedSource,
    dir: &Path,
    s3: Option<&ResolvedS3>,
    gcs: Option<&ResolvedGcs>,
    scratch: &Path,
    full_refresh: bool,
    classify_cache: &mut ClassifyCache,
    cell_name: &str,
) -> Result<BindOutcome> {
    let view = name.replace('"', "\"\"");
    // Only the `Some(inc)` incremental arm (deeply nested below) ever sets
    // this; a plain outer variable is simpler than threading it back up
    // through three levels of match-as-expression.
    let mut advance: Option<WatermarkAdvance> = None;
    // Raw/cell sources have no warehouse read to narrate — `None` fields
    // throughout, never a fabricated table/view/query kind or a zero row
    // count. Connection sources overwrite this per arm below.
    let no_connection_info = || SourceRunInfo {
        name: name.to_string(),
        connection: None,
        kind: None,
        staged_rows: None,
        bytes_scanned: None,
    };
    let info = match src {
        ResolvedSource::Raw(uri) => {
            let resolved = resolve_source_uri(uri, dir);
            conn.execute_batch(&format!(
                "CREATE OR REPLACE TEMP VIEW \"{view}\" AS SELECT * FROM '{}';",
                esc(&resolved)
            ))
            .with_context(|| format!("binding source '{name}' -> {resolved}"))?;
            tracing::info!(source = %name, uri = %resolved, "bound raw source");
            no_connection_info()
        }
        ResolvedSource::Cell {
            catalog,
            storage,
            table,
            version,
        } => {
            let alias = format!("__src_{idx}");
            // Mode by presence (ADR 0004 §12): a `catalog` in the cells map
            // attaches the upstream directly (local dev / self-managed);
            // otherwise the upstream's *published* artifact is fetched from
            // its storage prefix and attached locally — composing on
            // released, versioned state, not a live peer's internals.
            let (catalog, data_path) = match catalog {
                Some(c) => (resolve_catalog(c, dir)?, resolve_storage(storage, dir)?),
                None => {
                    if !crate::config::is_remote(storage) {
                        anyhow::bail!(
                            "cell source '{name}': `cells.…` has no `catalog:` (published \
                             mode), but its storage `{storage}` is not an object store"
                        );
                    }
                    let store = Store::for_storage(storage, s3, gcs)?;
                    let n = store.latest()?.with_context(|| {
                        format!(
                            "cell source '{name}': upstream has no published catalog \
                             (no catalog/LATEST under {storage})"
                        )
                    })?;
                    let src_dir = scratch.join(format!("src-{idx}"));
                    let local = store.download_execution(n, &src_dir)?;
                    tracing::info!(source = %name, execution = n, "fetched upstream artifact");
                    (
                        local.to_string_lossy().into_owned(),
                        format!("{storage}/data"),
                    )
                }
            };
            // OVERRIDE_DATA_PATH: trust the storage we were handed rather than the
            // absolute path A happened to record at build time (host/representation
            // differences shouldn't break a reference).
            conn.execute_batch(&format!(
                "ATTACH IF NOT EXISTS 'ducklake:{}' AS {alias} \
                 (DATA_PATH '{}', READ_ONLY, OVERRIDE_DATA_PATH true);",
                esc(&catalog),
                esc(&data_path)
            ))
            .with_context(|| format!("attaching cell source '{name}' ({catalog})"))?;
            let at = match version {
                Some(v) => format!(" AT (VERSION => {v})"),
                None => String::new(),
            };
            conn.execute_batch(&format!(
                "CREATE OR REPLACE TEMP VIEW \"{view}\" AS SELECT * FROM {alias}.\"{}\"{at};",
                table.replace('"', "\"\"")
            ))
            .with_context(|| format!("binding cell source '{name}' -> {table}"))?;
            tracing::info!(source = %name, table = %table, version = ?version, "bound cell source");
            no_connection_info()
        }
        ResolvedSource::Connection {
            connection,
            config,
            target,
            incremental,
        } => {
            let ty = config.type_name();
            // Alias keyed on the connection name + ATTACH IF NOT EXISTS: a
            // connection shared by several sources attaches once. Every
            // connection source needs the extension loaded and the catalog
            // attached, `table:` and `query:` alike — only what happens
            // next (classification, `qualify()`) is target-specific.
            let alias = format!("__conn_{connection}");
            conn.execute_batch(config.install_load_sql())
                .with_context(|| format!("loading DuckDB '{ty}' extension"))?;
            conn.execute_batch(&config.attach_sql(&alias))
                .with_context(|| format!("attaching connection '{connection}' ({ty})"))?;

            match target {
                // ADR 0007: author-owned server-side SQL, jobs-routed by
                // construction — there is no table to classify
                // (`classify_objects` skipped entirely) and no table path
                // to validate (`qualify()` skipped entirely). `incremental:`
                // on a `query:` source is refused at resolve time (§3); the
                // bail below is defense in depth against a resolve-time
                // regression silently dropping an author's `incremental:`
                // instead of failing loud.
                ConnectionTarget::Query(query) => {
                    if incremental.is_some() {
                        anyhow::bail!(
                            "source '{name}': `incremental:` on a `query:` source reached bind \
                             (internal error — this must be rejected at resolve time, ADR 0007 \
                             §3) — please report this"
                        );
                    }
                    // ADR 0007 §4: a dry-run preflight before the real read
                    // — free (no scan billed, `dry_run := true`). A clear
                    // query error fails loud here, pre-`BEGIN` and cheaper
                    // than the first staging read; anything else (a
                    // transport-ish hiccup, an error shape the narrow
                    // classifier doesn't recognize) only warns, and the
                    // real read below proceeds — it fails loud on its own
                    // if the query really is broken. Bytes are the
                    // narrated primitive; never a gate on size — but they
                    // are threaded into the run summary when we do get one.
                    let bytes_scanned = dry_run_preflight(conn, config, name, query)?;

                    // The connector's only transformation of the author's
                    // string is `esc()` for delivery — no identifier
                    // rewriting, no predicate injection (§2).
                    let select = config.query_read_sql(query);
                    let temp_table = format!("__jobs_{idx}");
                    let staging_uri = config.staging_uri();
                    let staged_rows = match conn
                        .execute_batch(&format!("CREATE TEMP TABLE {temp_table} AS {select};"))
                    {
                        Ok(()) => count_rows(conn, &temp_table, name)?,
                        // §3a's escalation applies unchanged: the export
                        // wraps the author's query instead of a generated
                        // `SELECT *`.
                        Err(e) if config.is_response_too_large(&e) && staging_uri.is_some() => {
                            stage_via_export(
                                conn,
                                name,
                                "query",
                                &temp_table,
                                cell_name,
                                staging_uri.expect("checked Some above"),
                                s3,
                                gcs,
                                |run_prefix| Ok(config.query_export_sql(query, run_prefix)),
                            )?
                        }
                        Err(e) => {
                            return Err(config.rewrite_response_too_large(
                                e,
                                name,
                                "query",
                                &format!("staging query source '{name}' via the jobs API"),
                            ))
                        }
                    };
                    // ADR 0007 §4: the known-hazard narration — a column the
                    // author expected numeric that BigQuery's own
                    // NUMERIC/BIGNUMERIC range degradation turned into
                    // VARCHAR becomes visible right here, not three
                    // transforms deep as `sum(VARCHAR)`.
                    narrate_staged_types(conn, name, &temp_table)?;
                    conn.execute_batch(&format!(
                        "CREATE OR REPLACE TEMP VIEW \"{view}\" AS SELECT * FROM {temp_table};"
                    ))
                    .with_context(|| format!("binding connection source '{name}' (query:)"))?;
                    tracing::info!(
                        source = %name, connection = %connection, staged_rows,
                        "source '{name}' is a query source — executing server-side via the \
                         BigQuery jobs API"
                    );
                    SourceRunInfo {
                        name: name.to_string(),
                        connection: Some(connection.clone()),
                        kind: Some("query".to_string()),
                        staged_rows: Some(staged_rows),
                        bytes_scanned,
                    }
                }
                ConnectionTarget::Table(table) => {
                    // Qualify first: it's pure validation, and
                    // INSTALL/ATTACH above may already have hit the network.
                    let qualified = config.qualify(&alias, table)?;

                    // View-backed sources: the attached catalog's Storage
                    // Read API cannot read a VIEW/MATERIALIZED
                    // VIEW/EXTERNAL table ("non-table entities cannot be
                    // read with the storage API"), and DuckDB's own
                    // information_schema misreports a real BigQuery view as
                    // BASE TABLE — so classification goes through the
                    // warehouse's own metadata, never the attach.
                    let meta = classify_cache
                        .classify(conn, connection, config, table)
                        .with_context(|| format!("classifying source '{name}' ({table})"))?;
                    let meta = match meta {
                        Some(m) => m,
                        None => {
                            // Classification denied (e.g. no
                            // `bigquery.jobs.create`): assume BASE TABLE
                            // (today's behavior), but probe the real read
                            // pre-BEGIN so a genuine view still fails at
                            // bind, not mid-transaction, with a rewritten
                            // error.
                            probe_storage_read(conn, config, name, table, &qualified)?;
                            ObjectMeta {
                                kind: ObjectKind::Table,
                                columns: IndexMap::new(),
                            }
                        }
                    };

                    match incremental {
                        None => match meta.kind {
                            ObjectKind::Table => {
                                // Byte-identical to the pre-view-routing SQL.
                                conn.execute_batch(&format!(
                                    "CREATE OR REPLACE TEMP VIEW \"{view}\" AS SELECT * FROM \
                                     {qualified};"
                                ))
                                .with_context(|| {
                                    format!("binding connection source '{name}' -> {table}")
                                })?;
                                tracing::info!(source = %name, connection = %connection, table = %table, "bound connection source");
                                SourceRunInfo {
                                    name: name.to_string(),
                                    connection: Some(connection.clone()),
                                    kind: Some("table".to_string()),
                                    staged_rows: None,
                                    bytes_scanned: None,
                                }
                            }
                            ObjectKind::Query => {
                                // Read-the-warehouse-exactly-once: stage the
                                // view's jobs-API read into a TEMP TABLE,
                                // then bind the source view over the staged
                                // copy, so N transforms referencing this
                                // source cost one BigQuery job, not N.
                                let select =
                                    config.read_sql(&alias, table, &meta, None).with_context(
                                        || format!("binding connection source '{name}' -> {table}"),
                                    )?;
                                let temp_table = format!("__jobs_{idx}");
                                let staging_uri = config.staging_uri();
                                let staged_rows = match conn.execute_batch(&format!(
                                    "CREATE TEMP TABLE {temp_table} AS {select};"
                                )) {
                                    Ok(()) => count_rows(conn, &temp_table, name)?,
                                    // ADR 0006 §3a: the exact ceiling
                                    // signature, and only that signature,
                                    // with a `staging_uri:` configured —
                                    // escalate to EXPORT DATA rather than
                                    // fail. Anything else re-raises
                                    // unchanged.
                                    Err(e)
                                        if config.is_response_too_large(&e)
                                            && staging_uri.is_some() =>
                                    {
                                        stage_via_export(
                                            conn,
                                            name,
                                            table,
                                            &temp_table,
                                            cell_name,
                                            staging_uri.expect("checked Some above"),
                                            s3,
                                            gcs,
                                            |run_prefix| {
                                                config.export_sql(table, &meta, None, run_prefix)
                                            },
                                        )?
                                    }
                                    Err(e) => {
                                        return Err(config.rewrite_response_too_large(
                                            e,
                                            name,
                                            table,
                                            &format!(
                                                "staging view source '{name}' -> {table} via \
                                                 the jobs API"
                                            ),
                                        ))
                                    }
                                };
                                narrate_staged_types(conn, name, &temp_table)?;
                                conn.execute_batch(&format!(
                                    "CREATE OR REPLACE TEMP VIEW \"{view}\" AS SELECT * FROM \
                                     {temp_table};"
                                ))
                                .with_context(|| {
                                    format!("binding connection source '{name}' -> {table}")
                                })?;
                                tracing::info!(
                                    source = %name, connection = %connection, table = %table, staged_rows,
                                    "source '{name}' is a view ({table}) — reading via the \
                                     BigQuery jobs API (full view materialized every run). Add \
                                     `incremental:` with a cursor to read only new rows."
                                );
                                SourceRunInfo {
                                    name: name.to_string(),
                                    connection: Some(connection.clone()),
                                    kind: Some("view".to_string()),
                                    staged_rows: Some(staged_rows),
                                    bytes_scanned: None,
                                }
                            }
                        },
                        Some(inc) => {
                            let adv = stage_incremental(
                                conn,
                                idx,
                                name,
                                &view,
                                table,
                                &qualified,
                                &alias,
                                config,
                                &meta,
                                inc,
                                full_refresh,
                                cell_name,
                                s3,
                                gcs,
                            )?;
                            let kind = match meta.kind {
                                ObjectKind::Table => "table",
                                ObjectKind::Query => "view",
                            };
                            let info = SourceRunInfo {
                                name: name.to_string(),
                                connection: Some(connection.clone()),
                                kind: Some(kind.to_string()),
                                staged_rows: Some(adv.staged_rows),
                                bytes_scanned: None,
                            };
                            advance = Some(adv);
                            info
                        }
                    }
                }
            }
        }
    };
    Ok(BindOutcome { advance, info })
}

/// The classification-denied fallback's safety net: probe the real table
/// read pre-BEGIN so a genuine view fails at bind (with a rewritten,
/// actionable error), instead of mid-transaction — the exact prod failure
/// this feature exists to fix. A no-op statement otherwise (LIMIT 1).
fn probe_storage_read(
    conn: &Connection,
    config: &ResolvedConnection,
    name: &str,
    table: &str,
    qualified: &str,
) -> Result<()> {
    let sql = format!("SELECT * FROM {qualified} LIMIT 1");
    let probe: std::result::Result<(), duckdb::Error> = (|| {
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        rows.next()?;
        Ok(())
    })();
    probe.map_err(|e| config.rewrite_view_leak(e, name, table))
}

// --- ADR 0006 §3a: the oversized-jobs-result escape hatch ------------------
//
// A jobs-path staging read can exceed BigQuery's ~10GB anonymous-result-table
// ceiling. Result size isn't classifiable up front (a view has no row
// count), so the only trigger is the ceiling error itself
// (`config.is_response_too_large`) — matched narrowly, so anything else
// re-raises unchanged. On that exact signature, a connection with
// `staging_uri:` escalates: `EXPORT DATA` writes the *same* query's result
// as parquet to a run-scoped scratch prefix, the engine stages from the
// parquet, then deletes the prefix. The connector owns one more pure
// renderer (`export_sql`); this function owns the sequencing and side
// effects, same purity split as the rest of the connector seam.

/// `SELECT count(*)` from a staged jobs-path temp table, shared by the
/// plain-read success path and `stage_via_export`'s post-EXPORT count so the
/// two can't drift on how "staged rows" is defined. Clamped to non-negative
/// (defense in depth; `count(*)` never actually returns one).
fn count_rows(conn: &Connection, temp_table: &str, name: &str) -> Result<u64> {
    let n: i64 = conn
        .query_row(&format!("SELECT count(*) FROM {temp_table}"), [], |r| {
            r.get(0)
        })
        .with_context(|| format!("counting staged rows for source '{name}'"))?;
    Ok(n.max(0) as u64)
}

/// Run the §3a escalation for one jobs-path staging site: the plain
/// `__jobs_<idx>` arm (a view-routed `table:` source, ADR 0006 — or an
/// author-owned `query:` source, ADR 0007), or `stage_incremental`'s
/// `__delta_<idx>` arm. Returns the staged row count. `describe` names what
/// is being read, for narration and errors only — a table path for
/// `table:`/view sources, `"query"` for a `query:` source (there is no
/// table name to name there). `build_call_sql` is the connector's pure
/// renderer for this source's `EXPORT DATA` statement given the run prefix
/// (`ResolvedConnection::export_sql` for `table:`/view sources,
/// `::query_export_sql` for `query:` sources — ADR 0007 §2: the export
/// wraps the author's query verbatim) — captured as a closure so this
/// function stays agnostic to which kind of source it is staging. Pre-
/// `BEGIN`, like the read it replaces: a failed export or read fails the
/// bind loudly. Cleanup is the one best-effort step — see
/// `cleanup_export_prefix`.
#[allow(clippy::too_many_arguments)]
fn stage_via_export(
    conn: &Connection,
    name: &str,
    describe: &str,
    temp_table: &str,
    cell_name: &str,
    staging_uri: &str,
    s3: Option<&ResolvedS3>,
    gcs: Option<&ResolvedGcs>,
    build_call_sql: impl FnOnce(&str) -> Result<String>,
) -> Result<u64> {
    // Run-scoped, not just source-scoped: two sources hitting the ceiling in
    // the same run (or a retried run) must never collide on one prefix.
    // `unique_suffix` is the exact discipline `new_scratch_dir` uses for its
    // local scratch directories — process id + a monotonic counter, so a
    // crash-orphaned prefix is identifiable after the fact.
    let rel_prefix = format!("{}/{}", sanitize_tag(cell_name), unique_suffix());
    let run_prefix = format!("{}/{}", staging_uri.trim_end_matches('/'), rel_prefix);

    tracing::info!(
        source = %name, describe = %describe, prefix = %run_prefix,
        "source '{name}' ({describe}) result exceeded BigQuery's response ceiling — staging via \
         EXPORT DATA to {run_prefix}"
    );

    let call_sql = build_call_sql(&run_prefix).with_context(|| {
        format!("building the EXPORT DATA statement for source '{name}' ({describe})")
    })?;

    // query_row, not execute_batch: the outcome (success, bytes billed) is a
    // row datamk must inspect, not a side effect it can fire-and-forget.
    let export = conn.query_row(&call_sql, [], |r| {
        Ok((r.get::<_, bool>(0)?, r.get::<_, Option<i64>>(5)?))
    });
    let (success, total_bytes_processed) = match export {
        Ok(row) => row,
        Err(e) => {
            cleanup_export_prefix(staging_uri, &rel_prefix, s3, gcs, name);
            return Err(anyhow::Error::new(e).context(format!(
                "EXPORT DATA job failed for source '{name}' ({describe}) while staging an \
                 oversized jobs-API result to {run_prefix}"
            )));
        }
    };
    if !success {
        cleanup_export_prefix(staging_uri, &rel_prefix, s3, gcs, name);
        anyhow::bail!(
            "EXPORT DATA job for source '{name}' ({describe}) reported failure (success = \
             false) while staging to {run_prefix}"
        );
    }

    // Reads gs:// through the engine's existing GCS setup (the profile's
    // `gcs:` block / vendored extension / HMAC secret) — `staging_uri`
    // deliberately adds no separate auth plumbing. If that setup isn't
    // configured for this cell, this fails with DuckDB's own GCS error, and
    // the context below names the reason.
    let read = conn.execute_batch(&format!(
        "CREATE TEMP TABLE {temp_table} AS SELECT * FROM read_parquet('{}/part-*.parquet');",
        esc(&run_prefix)
    ));
    // Best-effort cleanup between export and read too — a read failure must
    // not leave the exported parquet behind any more than a success would.
    cleanup_export_prefix(staging_uri, &rel_prefix, s3, gcs, name);
    read.with_context(|| {
        format!(
            "reading the EXPORT DATA parquet for source '{name}' ({describe}) back from \
             {run_prefix} — staging_uri requires this cell's profile to have GCS read access \
             configured (a `gcs:` block, or ambient credentials httpfs can use)"
        )
    })?;

    let staged_rows = count_rows(conn, temp_table, name)
        .with_context(|| "counting staged rows after EXPORT DATA".to_string())?;
    tracing::info!(
        source = %name, staged_rows, total_bytes_processed = ?total_bytes_processed,
        "staged oversized jobs-API result for source '{name}' via EXPORT DATA"
    );
    Ok(staged_rows)
}

/// Best-effort deletion of an `EXPORT DATA` run prefix. Failure here is a
/// `warn`, never a run failure: `stage_via_export`'s `rel_prefix` is
/// run-scoped (process id + a monotonic counter), so an orphan is
/// identifiable after the fact and safe to remove by hand, or via an
/// object-lifecycle rule on `staging_uri` (ADR 0006 §3a's suggested
/// belt-and-braces).
fn cleanup_export_prefix(
    staging_uri: &str,
    rel_prefix: &str,
    s3: Option<&ResolvedS3>,
    gcs: Option<&ResolvedGcs>,
    name: &str,
) {
    let result = Store::for_storage(staging_uri, s3, gcs).and_then(|s| s.delete_prefix(rel_prefix));
    if let Err(e) = result {
        tracing::warn!(
            source = %name, staging_uri, prefix = %rel_prefix, error = %e,
            "failed to clean up EXPORT DATA scratch prefix {staging_uri}/{rel_prefix} — orphaned \
             parquet left behind (run-scoped and identifiable by this prefix; safe to delete by \
             hand, or set an object-lifecycle rule on {staging_uri})"
        );
    }
}

// --- ADR 0007 §4: query: dry-run preflight + staged-type narration --------
//
// Two honest-limits-of-engine-help mechanisms for a `query:` source's
// offline-unvalidatable body: a free (`dry_run := true`) bind-time preflight
// that fails loud on a clear query error and narrates the real read's exact
// scan cost, and a post-staging DESCRIBE that surfaces the one hazard the
// engine cannot detect authoritatively (BigQuery NUMERIC/BIGNUMERIC beyond
// DuckDB's range degrading silently to VARCHAR).

/// Run the dry-run preflight (ADR 0007 §4) for a `query:` source's `query`,
/// pre-`BEGIN`. `Ok(())` on a successful dry run (after narrating its exact
/// `total_bytes_processed`) or on a failure the narrow classifier doesn't
/// recognize as a query error (after a `warn`, so the real read still
/// runs); `Err` only for a failure that unambiguously names a bad query.
/// Returns `Ok(Some(total_bytes_processed))` on a successful dry run — the
/// value threaded into the published run summary's `bytes_scanned`
/// (`SourceRunInfo`) as well as narrated here — or `Ok(None)` when the dry
/// run itself failed in a way that only warns (no byte count to report,
/// but the real read still proceeds).
fn dry_run_preflight(
    conn: &Connection,
    config: &ResolvedConnection,
    name: &str,
    query: &str,
) -> Result<Option<i64>> {
    let dry_run_sql = config.query_dry_run_sql(query);
    match conn.query_row(&dry_run_sql, [], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<bool>>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    }) {
        Ok((total_bytes_processed, cache_hit, location)) => {
            tracing::info!(
                source = %name, total_bytes_processed, cache_hit = ?cache_hit, location = ?location,
                "source '{name}' (query:) dry-run preflight: {total_bytes_processed} bytes will \
                 be scanned by the real read (any dollar figure is an on-demand estimate — bytes \
                 are the narrated primitive)"
            );
            Ok(Some(total_bytes_processed))
        }
        Err(e) if config.is_dry_run_query_error(&e) => Err(anyhow::Error::new(e).context(format!(
            "dry-run preflight for query: source '{name}' failed — the query itself looks \
                 invalid (see the BigQuery message above); fix `query:` and re-run"
        ))),
        Err(e) => {
            tracing::warn!(
                source = %name, error = %e,
                "dry-run preflight for query: source '{name}' failed in a way that doesn't look \
                 like a query error — proceeding to the real read, which fails loud on its own \
                 if the query is genuinely broken"
            );
            Ok(None)
        }
    }
}

/// ADR 0007 §4: log a staged jobs-path temp table's column types — the
/// honest ceiling of engine help for the BigQuery-numeric-degrades-to-
/// VARCHAR hazard: a column the author expected numeric surfacing as
/// VARCHAR becomes visible here, at the source boundary, instead of three
/// transforms deep as `sum(VARCHAR)`. One compact line, `name:type` pairs,
/// so it reads at a glance in the run log. Used for `query:` sources
/// (§4's stated case) and, since the DESCRIBE is the same one call either
/// way, every other jobs-routed staging site too (a view, plain or
/// incremental).
fn narrate_staged_types(conn: &Connection, name: &str, temp_table: &str) -> Result<()> {
    let cols = describe_columns(conn, temp_table)
        .with_context(|| format!("describing staged types for source '{name}'"))?;
    let pairs = cols
        .iter()
        .map(|(c, t, _)| format!("{c}:{t}"))
        .collect::<Vec<_>>()
        .join(", ");
    tracing::info!(
        source = %name, columns = %pairs,
        "staged column types for source '{name}': {pairs}"
    );
    Ok(())
}

// --- ADR 0005: incremental source loading (watermarked executions) --------
//
// The engine's new promise: a `connection` source with an `incremental:`
// block reads only rows past a persisted high-water mark. See
// docs/adr/0005-incremental-source-loading.md §1 for the design; this
// section is the bind path plus the in-transaction watermark persist, the
// shrink detector, and `--verify-replay`.

/// Cursor types the engine understands. A closed set, classified from the
/// live column's actual DuckDB type at bind time (§1) — offline `verify`
/// cannot check this; there is no live warehouse column to look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorType {
    Timestamp,
    Date,
    Integer,
}

/// A watermark value, kept as a DuckDB-rendered literal (never Rust datetime
/// math) so it round-trips through `__datamk_watermarks`'s typed columns
/// without ever passing through an untyped string comparison. `Ts`/`Date` are
/// rendered via `::VARCHAR` from the session time zone, which `setup`/`
/// open_artifact` pin to UTC (`SET TimeZone`) so a `Ts` value is always an
/// ISO-8601 UTC-offset string regardless of host locale.
///
/// `pub(crate)`, with no dialect flag: `engine::connectors::bigquery` renders
/// its own GoogleSQL literal from `(MarkValue, BQ-native data_type)` rather
/// than this type growing a per-connector rendering mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MarkValue {
    Ts(String),
    Date(String),
    Int(i64),
}

impl MarkValue {
    /// Render as a typed SQL literal for use in a predicate or an upsert.
    /// `esc()` is applied to the string variants — defense in depth: a
    /// DuckDB-rendered timestamp/date never contains a quote, but the
    /// predicate this feeds is built by string formatting, not a parameter,
    /// so it must not be possible for a crafted value to escape the literal.
    pub(crate) fn as_literal(&self) -> String {
        match self {
            MarkValue::Ts(s) => format!("TIMESTAMPTZ '{}'", esc(s)),
            MarkValue::Date(s) => format!("DATE '{}'", esc(s)),
            MarkValue::Int(n) => n.to_string(),
        }
    }
}

impl fmt::Display for MarkValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarkValue::Ts(s) | MarkValue::Date(s) => write!(f, "{s}"),
            MarkValue::Int(n) => write!(f, "{n}"),
        }
    }
}

/// One source's watermark advance, computed pre-BEGIN and threaded by `run`
/// into the transform transaction for the actual persist (§1 step 5).
struct WatermarkAdvance {
    source: String,
    cursor: String,
    ty: CursorType,
    /// `max(cursor)` over the staged delta; `None` when the delta was empty
    /// — `run`'s persist step must skip these sources entirely (the
    /// `greatest(old, NULL)` guard).
    new_max: Option<MarkValue>,
    staged_rows: u64,
}

/// Classify a live DuckDB column type string into the closed cursor-type set.
/// `None` ⇒ not a supported cursor type.
fn classify_cursor_type(actual_type: &str) -> Option<CursorType> {
    let t = actual_type.to_uppercase();
    if t.starts_with("TIMESTAMP") {
        Some(CursorType::Timestamp)
    } else if t == "DATE" {
        Some(CursorType::Date)
    } else if matches!(
        t.as_str(),
        "TINYINT"
            | "SMALLINT"
            | "INTEGER"
            | "BIGINT"
            | "HUGEINT"
            | "UTINYINT"
            | "USMALLINT"
            | "UINTEGER"
            | "UBIGINT"
            | "UHUGEINT"
    ) {
        Some(CursorType::Integer)
    } else {
        None
    }
}

/// `(column_name, duckdb_type, nullable)` for every column of a table
/// expression, via `DESCRIBE SELECT * FROM <qualified>` — a plain `DESCRIBE
/// <qualified>` doesn't parse for a dotted/aliased multi-part reference (a
/// connector's `qualify()` output), so this is the form that works uniformly.
fn describe_columns(conn: &Connection, qualified: &str) -> Result<Vec<(String, String, bool)>> {
    let mut stmt = conn.prepare(&format!("DESCRIBE SELECT * FROM {qualified}"))?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (name, ty, null) = r?;
        // DuckDB's DESCRIBE always populates this column; an unexpected value
        // (a scanner surfacing something other than YES/NO) degrades to
        // "not nullable" rather than failing the bind — the warning below is
        // an aid, never a safety net.
        out.push((name, ty, null.eq_ignore_ascii_case("YES")));
    }
    Ok(out)
}

fn find_column<'a>(
    cols: &'a [(String, String, bool)],
    cursor: &str,
) -> Option<&'a (String, String, bool)> {
    cols.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(cursor))
}

/// §1 cursor existence/type errors, exact text.
fn validate_cursor(
    name: &str,
    table: &str,
    cursor: &str,
    cols: &[(String, String, bool)],
) -> Result<(CursorType, String, bool)> {
    let Some((_, actual_ty, nullable)) = find_column(cols, cursor) else {
        let available = cols
            .iter()
            .map(|(n, _, _)| n.clone())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "source '{name}': incremental cursor '{cursor}' does not exist in {table}. Columns \
             available: {available}. Set `cursor:` to one of these, or drop `incremental:` to \
             full-scan this source."
        );
    };
    match classify_cursor_type(actual_ty) {
        Some(ty) => Ok((ty, actual_ty.clone(), *nullable)),
        None => anyhow::bail!(
            "source '{name}': incremental cursor '{cursor}' is {actual_ty}; a cursor must be a \
             timestamp, date, or integer column. Point `cursor:` at a monotonic column of one of \
             those types, or drop `incremental:` to full-scan this source."
        ),
    }
}

/// Reconstruct a canonical duration string (`2h`, `30m`, …) from a `Duration`
/// for error text — `ResolvedIncremental` keeps only the parsed value, not
/// the author's original spelling, so this is the closest displayable
/// equivalent (functionally identical, not necessarily verbatim).
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs > 0 && secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs > 0 && secs.is_multiple_of(3_600) {
        format!("{}h", secs / 3_600)
    } else if secs > 0 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// §1 lookback/cursor-type compatibility, checked at bind time (the type is
/// unknown offline). Returns the SQL `INTERVAL` fragment to subtract from the
/// watermark when lookback applies, `None` otherwise. Date cursors accept
/// lookback only when it is a whole number of days — a sub-day lookback
/// against a DATE column has no meaning to shift by, so it is refused rather
/// than silently truncated.
fn validate_lookback(
    name: &str,
    cursor: &str,
    ty: CursorType,
    actual_ty: &str,
    lookback: Option<Duration>,
) -> Result<Option<String>> {
    let Some(d) = lookback else {
        return Ok(None);
    };
    match ty {
        CursorType::Timestamp => Ok(Some(format!("INTERVAL '{}' SECOND", d.as_secs()))),
        CursorType::Integer => anyhow::bail!(
            "source '{name}': `lookback: {}` needs a time-typed cursor, but cursor '{cursor}' is \
             {actual_ty}. Remove `lookback` — integer cursors have no time window — or set \
             `cursor:` to a timestamp or date column.",
            format_duration(d)
        ),
        CursorType::Date => {
            let secs = d.as_secs();
            if !secs.is_multiple_of(86_400) {
                anyhow::bail!(
                    "source '{name}': `lookback: {}` needs a time-typed cursor, but cursor \
                     '{cursor}' is {actual_ty}. Remove `lookback` — a date cursor's lookback \
                     must be a whole number of days — or set `cursor:` to a timestamp column.",
                    format_duration(d)
                );
            }
            Ok(Some(format!("INTERVAL '{}' DAY", secs / 86_400)))
        }
    }
}

/// Whether `__datamk_watermarks` exists yet in `lake` — `false` for a
/// pre-ADR-0005 artifact or one that has never staged an incremental source.
/// `pub(crate)`: `ops.rs` reuses this for the `status`/`rollback` watermark
/// narration (ADR 0005 §4) rather than re-deriving the same check.
pub(crate) fn watermark_table_exists(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM information_schema.tables \
         WHERE table_catalog = 'lake' AND table_name = '__datamk_watermarks'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// R3: the read side must never trust a corrupt catalog silently. Exactly one
/// row per source is the invariant the upsert step (§1 step 5) depends on;
/// this fails loud, naming the table and the offending sources, rather than
/// picking an arbitrary `max()` over duplicate rows. `pub(crate)`: `ops.rs`
/// reuses this exact check (and its exact error text) before narrating
/// watermark state — a corrupt table must fail loud there too, not just
/// during `run` (ADR 0005 §4).
pub(crate) fn check_watermark_duplicates(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT source FROM __datamk_watermarks GROUP BY source HAVING count(*) > 1 \
         ORDER BY source",
    )?;
    let dups: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<_, _>>()?;
    if !dups.is_empty() {
        anyhow::bail!(
            "__datamk_watermarks is corrupt: duplicate rows for source(s) {} — exactly one row \
             per source is expected. Fix this manually (delete the extras) before the next run; \
             the watermark for an affected source cannot be trusted until it does.",
            dups.join(", ")
        );
    }
    Ok(())
}

/// Read the effective lower bound for `source` — `max(mark)` adjusted by
/// lookback, computed entirely in SQL (never Rust datetime math) so the
/// arithmetic is exact for whatever calendar/timezone rules the type implies.
/// `None` ⇒ no watermark row for this source (bootstrap for this source).
fn read_watermark(
    conn: &Connection,
    source: &str,
    ty: CursorType,
    lookback_sql: Option<&str>,
) -> Result<Option<MarkValue>> {
    let esc_source = esc(source);
    match ty {
        CursorType::Timestamp => {
            let expr = match lookback_sql {
                Some(iv) => format!("(max(mark_ts) - {iv})::VARCHAR"),
                None => "max(mark_ts)::VARCHAR".to_string(),
            };
            let v: Option<String> = conn.query_row(
                &format!("SELECT {expr} FROM __datamk_watermarks WHERE source = '{esc_source}'"),
                [],
                |r| r.get(0),
            )?;
            Ok(v.map(MarkValue::Ts))
        }
        CursorType::Date => {
            let expr = match lookback_sql {
                Some(iv) => format!("(max(mark_date) - {iv})::DATE::VARCHAR"),
                None => "max(mark_date)::VARCHAR".to_string(),
            };
            let v: Option<String> = conn.query_row(
                &format!("SELECT {expr} FROM __datamk_watermarks WHERE source = '{esc_source}'"),
                [],
                |r| r.get(0),
            )?;
            Ok(v.map(MarkValue::Date))
        }
        CursorType::Integer => {
            let v: Option<i64> = conn.query_row(
                &format!(
                    "SELECT max(mark_int) FROM __datamk_watermarks WHERE source = '{esc_source}'"
                ),
                [],
                |r| r.get(0),
            )?;
            Ok(v.map(MarkValue::Int))
        }
    }
}

/// `count(*)` and `max(cursor)` over the just-staged delta table. `new_max`
/// is `None` when the delta is empty, regardless of what `max()` over zero
/// rows already gives — explicit, so the empty-delta skip downstream never
/// depends on a NULL-propagation subtlety.
fn compute_advance(
    conn: &Connection,
    idx: usize,
    cursor: &str,
    ty: CursorType,
) -> Result<(u64, Option<MarkValue>)> {
    let cq = cursor.replace('"', "\"\"");
    let (n, mark): (i64, Option<MarkValue>) = match ty {
        CursorType::Timestamp => {
            let (n, m): (i64, Option<String>) = conn.query_row(
                &format!("SELECT count(*), max(\"{cq}\")::VARCHAR FROM __delta_{idx}"),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            (n, m.map(MarkValue::Ts))
        }
        CursorType::Date => {
            let (n, m): (i64, Option<String>) = conn.query_row(
                &format!("SELECT count(*), max(\"{cq}\")::VARCHAR FROM __delta_{idx}"),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            (n, m.map(MarkValue::Date))
        }
        CursorType::Integer => {
            let (n, m): (i64, Option<i64>) = conn.query_row(
                &format!("SELECT count(*), max(\"{cq}\") FROM __delta_{idx}"),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            (n, m.map(MarkValue::Int))
        }
    };
    Ok((n.max(0) as u64, if n > 0 { mark } else { None }))
}

/// Bind an incremental connection source (§1 steps 1-4; step 5, the persist,
/// happens later in `run` inside the transform transaction).
///
/// INVARIANT: every statement below runs before `BEGIN` and must be read-only
/// against `lake` — an auto-committed write here would create a second
/// DuckLake snapshot and break the one-snapshot-per-execution invariant this
/// engine is built on. Table creation for `__datamk_watermarks` is deferred
/// to `persist_watermarks`, inside the transaction.
#[allow(clippy::too_many_arguments)]
fn stage_incremental(
    conn: &Connection,
    idx: usize,
    name: &str,
    view: &str,
    table: &str,
    qualified: &str,
    alias: &str,
    config: &ResolvedConnection,
    meta: &ObjectMeta,
    inc: &ResolvedIncremental,
    full_refresh: bool,
    cell_name: &str,
    s3: Option<&ResolvedS3>,
    gcs: Option<&ResolvedGcs>,
) -> Result<WatermarkAdvance> {
    // Cursor existence/type/nullability validation stays on `describe_columns`
    // through the attach: `DESCRIBE SELECT * FROM <qualified>` is
    // REST-metadata-only and works uniformly whether the object is a table or
    // a view — it is never routed through the jobs API.
    let cols = describe_columns(conn, qualified)
        .with_context(|| format!("describing source '{name}' ({table}) for incremental bind"))?;
    let (ty, actual_ty, nullable) = validate_cursor(name, table, &inc.cursor, &cols)?;
    let lookback_sql = validate_lookback(name, &inc.cursor, ty, &actual_ty, inc.lookback)?;

    if nullable {
        let cursor = &inc.cursor;
        tracing::warn!(
            "source '{name}': cursor '{cursor}' is nullable — rows with a NULL {cursor} are \
             staged once at bootstrap and never seen again (NULL is excluded by `{cursor} > \
             watermark`). Make the column NOT NULL upstream if those rows matter."
        );
    }

    let mark = if full_refresh {
        None
    } else if watermark_table_exists(conn)? {
        check_watermark_duplicates(conn)?;
        read_watermark(conn, name, ty, lookback_sql.as_deref())?
    } else {
        None
    };

    // The watermark (already lookback-adjusted by `read_watermark`) is
    // baked into the jobs-API query itself — the connector never sees
    // lookback, only the effective lower bound. Computed up front (not just
    // inside the `ObjectKind::Query` arm below) because the ADR 0006 §3a
    // escalation needs the identical predicate the failed jobs-API read was
    // built with.
    let predicate = mark.as_ref().map(|m| CursorPredicate {
        cursor: &inc.cursor,
        mark: m,
    });

    let stage_sql = match meta.kind {
        ObjectKind::Table => {
            // Byte-identical to the pre-view-routing staging SQL.
            let cq = inc.cursor.replace('"', "\"\"");
            match &mark {
                Some(m) => format!(
                    "CREATE TEMP TABLE __delta_{idx} AS SELECT * FROM {qualified} WHERE \"{cq}\" \
                     > {};",
                    m.as_literal()
                ),
                None => format!("CREATE TEMP TABLE __delta_{idx} AS SELECT * FROM {qualified};"),
            }
        }
        ObjectKind::Query => {
            let select = config
                .read_sql(alias, table, meta, predicate.as_ref())
                .with_context(|| {
                    format!("binding incremental source '{name}' -> {table} via the jobs API")
                })?;
            format!("CREATE TEMP TABLE __delta_{idx} AS {select};")
        }
    };
    let temp_table = format!("__delta_{idx}");
    let staging_uri = config.staging_uri();
    match conn.execute_batch(&stage_sql) {
        Ok(()) => {}
        // ADR 0006 §3a: the exact ceiling signature, and only that
        // signature, with a `staging_uri:` configured — escalate to EXPORT
        // DATA rather than fail. Anything else re-raises unchanged. The
        // returned row count is discarded: `compute_advance` below counts
        // the staged delta itself regardless of which path staged it, so
        // the two counts can never disagree.
        Err(e) if config.is_response_too_large(&e) && staging_uri.is_some() => {
            stage_via_export(
                conn,
                name,
                table,
                &temp_table,
                cell_name,
                staging_uri.expect("checked Some above"),
                s3,
                gcs,
                |run_prefix| config.export_sql(table, meta, predicate.as_ref(), run_prefix),
            )?;
        }
        Err(e) => {
            return Err(config.rewrite_response_too_large(
                e,
                name,
                table,
                &format!("staging incremental delta for source '{name}'"),
            ))
        }
    }
    // ADR 0007 §4's staged-type narration, extended to jobs-routed
    // incremental view staging since the DESCRIBE is the same call either
    // way — a `Table`-kind delta already has known types from
    // classification and doesn't need it.
    if meta.kind == ObjectKind::Query {
        narrate_staged_types(conn, name, &temp_table)?;
    }
    conn.execute_batch(&format!(
        "CREATE OR REPLACE TEMP VIEW \"{view}\" AS SELECT * FROM {temp_table};"
    ))
    .with_context(|| format!("binding incremental source '{name}' -> {table}"))?;

    let (staged_rows, new_max) = compute_advance(conn, idx, &inc.cursor, ty)?;

    match (&mark, meta.kind) {
        (Some(m), ObjectKind::Query) => tracing::info!(
            source = %name, staged_rows, watermark = %m,
            "source '{name}' is a view ({table}) — reading via the BigQuery jobs API with the \
             watermark predicate baked into the job."
        ),
        (Some(m), ObjectKind::Table) => tracing::info!(
            source = %name, staged_rows, watermark = %m,
            "staged delta past watermark"
        ),
        (None, _) => tracing::info!(
            source = %name, staged_rows,
            "staged full table (bootstrap)"
        ),
    }

    Ok(WatermarkAdvance {
        source: name.to_string(),
        cursor: inc.cursor.clone(),
        ty,
        new_max,
        staged_rows,
    })
}

/// §1 step 5: persist the collected advances inside the transform
/// transaction, after transforms succeed and before COMMIT. A source with an
/// empty delta is skipped entirely — the explicit guard so
/// `greatest(old, NULL)` can never run and erase a watermark.
fn persist_watermarks(
    conn: &Connection,
    advances: &[WatermarkAdvance],
    full_refresh: bool,
) -> Result<()> {
    if advances.is_empty() {
        return Ok(());
    }
    if !full_refresh && advances.iter().all(|a| a.new_max.is_none()) {
        return Ok(());
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS __datamk_watermarks ( \
           source VARCHAR NOT NULL, cursor_column VARCHAR NOT NULL, \
           mark_ts TIMESTAMPTZ, mark_date DATE, mark_int BIGINT, \
           last_delta_rows BIGINT \
         );",
    )
    .context("creating __datamk_watermarks")?;

    for adv in advances {
        let Some(new_max) = &adv.new_max else {
            continue; // empty delta -> no write at all
        };
        upsert_watermark(conn, adv, new_max, full_refresh)?;
    }
    Ok(())
}

fn upsert_watermark(
    conn: &Connection,
    adv: &WatermarkAdvance,
    new_max: &MarkValue,
    full_refresh: bool,
) -> Result<()> {
    let esc_source = esc(&adv.source);
    let esc_cursor = esc(&adv.cursor);
    // Derived from `adv.ty` (not from `new_max`'s variant) so the watermark
    // column is always the one the cursor was actually classified as, even
    // if a future refactor separates how `new_max` is produced.
    let col = match adv.ty {
        CursorType::Timestamp => "mark_ts",
        CursorType::Date => "mark_date",
        CursorType::Integer => "mark_int",
    };
    let lit = new_max.as_literal();
    // COALESCE handles the case where the existing row's mark column is NULL
    // (e.g. a fresh row, or the cursor's type changed across a full-refresh)
    // regardless of whether DuckDB's `greatest` is NULL-safe — `lit` is never
    // NULL here (guarded by the empty-delta skip above).
    let new_expr = if full_refresh {
        lit.clone()
    } else {
        format!("COALESCE(greatest({col}, {lit}), {lit})")
    };

    let updated = conn.execute(
        &format!(
            "UPDATE __datamk_watermarks SET {col} = {new_expr}, cursor_column = '{esc_cursor}', \
             last_delta_rows = {rows} WHERE source = '{esc_source}';",
            rows = adv.staged_rows
        ),
        [],
    )?;
    if updated == 0 {
        conn.execute_batch(&format!(
            "INSERT INTO __datamk_watermarks (source, cursor_column, {col}, last_delta_rows) \
             VALUES ('{esc_source}', '{esc_cursor}', {lit}, {rows});",
            rows = adv.staged_rows
        ))
        .with_context(|| format!("inserting watermark row for source '{}'", adv.source))?;
    }
    Ok(())
}

/// Every non-reserved table in `lake` (excludes `__datamk_%`), for the shrink
/// detector and `--verify-replay`. `\_\_datamk\_%` with `ESCAPE '\'` (R8): a
/// bare `_` is a LIKE wildcard, so an unescaped pattern over-matches (e.g.
/// `ab_datamk_x`).
fn list_lake_tables(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_catalog = 'lake' AND table_schema = 'main' \
         AND table_name NOT LIKE '\\_\\_datamk\\_%' ESCAPE '\\' \
         ORDER BY table_name",
    )?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(names)
}

fn snapshot_table_counts(conn: &Connection) -> Result<IndexMap<String, i64>> {
    let mut out = IndexMap::new();
    for t in list_lake_tables(conn)? {
        let n: i64 = conn.query_row(
            &format!("SELECT count(*) FROM \"{}\"", t.replace('"', "\"\"")),
            [],
            |r| r.get(0),
        )?;
        out.insert(t, n);
    }
    Ok(out)
}

/// Formats the shrink detector's findings (ADR 0005 §2 item 2) into the
/// un-missable end-of-run block `run` prints via `eprintln!` (see `run`'s
/// tail). Pure and unit-testable, mirroring `format_rollback_lines` in
/// `ops.rs`. Advisory tone throughout — this never fails the run.
fn format_shrink_summary_lines(shrunk: &[(String, i64, i64)]) -> Vec<String> {
    let mut lines = vec![
        String::new(),
        format!(
            "WARNING: {} table{} shrank during this run:",
            shrunk.len(),
            if shrunk.len() == 1 { "" } else { "s" }
        ),
    ];
    for (table, before, after) in shrunk {
        lines.push(format!(
            "  {table}: {} -> {} rows",
            crate::ops::group_thousands(*before),
            crate::ops::group_thousands(*after)
        ));
    }
    lines.push(
        "  If any of these tables accumulate from an incremental source, the transform \
         likely replaced history with the delta instead of merging into it — see \
         docs/guides/incremental.md"
            .to_string(),
    );
    lines
}

/// Pure comparator (unit-testable without a connection): tables present in
/// both snapshots whose row count decreased.
fn shrunk_tables(
    before: &IndexMap<String, i64>,
    after: &IndexMap<String, i64>,
) -> Vec<(String, i64, i64)> {
    before
        .iter()
        .filter_map(|(t, &b)| after.get(t).filter(|&&a| a < b).map(|&a| (t.clone(), b, a)))
        .collect()
}

const VERIFY_REPLAY_TAIL: &str = "Incremental sources deliver at-least-once, so a re-delivered \
row must not change the output — use an anti-join or MERGE, never `CREATE OR REPLACE`. If a \
transform is intentionally non-deterministic (now(), random()), --verify-replay cannot pass. See \
docs/guides/incremental.md.";

/// `--verify-replay` (ADR 0005 §2 item 1): re-run the transform sequence
/// against the identical staged delta inside a transaction it then rolls
/// back, and fail if any output table's contents changed. Detects
/// DUPLICATION (a plain `INSERT ... SELECT`); truncation is structurally
/// idempotent and therefore invisible here (see the shrink detector).
fn verify_replay(cell: &Cell, full_refresh: bool) -> Result<()> {
    let tables = list_lake_tables(&cell.conn)?;
    if tables.is_empty() {
        return Ok(());
    }

    // Snapshot pre-replay state into TEMP tables — the native temp schema,
    // never `lake`, so these never trip the reserved-prefix check.
    for (i, t) in tables.iter().enumerate() {
        cell.conn
            .execute_batch(&format!(
                "CREATE TEMP TABLE __replay_snap_{i} AS SELECT * FROM \"{}\";",
                t.replace('"', "\"\"")
            ))
            .with_context(|| format!("snapshotting table '{t}' for --verify-replay"))?;
    }

    cell.conn
        .execute_batch("BEGIN")
        .context("begin verify-replay transaction")?;
    let replay: Result<()> = (|| {
        // Dispatches through the exact same composed DML `run` used (ADR
        // 0008 §5b): a declarative entry replays its stage/guard/strategy
        // sequence, not just a raw file's text, so a real `MERGE` is what
        // gets diffed for replay-safety, not a no-op stand-in.
        for t in &cell.transforms {
            execute_transform(&cell.conn, &cell.dir, t, full_refresh).with_context(|| {
                format!(
                    "re-executing transform {} for --verify-replay",
                    t.file_path()
                )
            })?;
        }
        Ok(())
    })();

    if let Err(e) = replay {
        cell.conn
            .execute_batch("ROLLBACK")
            .context("rolling back verify-replay transaction")?;
        return Err(e.context(
            "verify-replay: re-running the transform pipeline failed. Transforms must be \
             re-runnable — they re-execute every scheduled run; a transform that cannot re-run \
             is broken independent of incremental sources.",
        ));
    }

    let mut diffs = Vec::new();
    for (i, t) in tables.iter().enumerate() {
        let quoted = t.replace('"', "\"\"");
        let (added, removed): (i64, i64) = cell
            .conn
            .query_row(
                &format!(
                    "SELECT \
                       (SELECT count(*) FROM (SELECT * FROM \"{quoted}\" EXCEPT ALL SELECT * FROM __replay_snap_{i})), \
                       (SELECT count(*) FROM (SELECT * FROM __replay_snap_{i} EXCEPT ALL SELECT * FROM \"{quoted}\"))"
                ),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .with_context(|| format!("comparing table '{t}' for --verify-replay"))?;
        if added != 0 || removed != 0 {
            let before: i64 = cell.conn.query_row(
                &format!("SELECT count(*) FROM __replay_snap_{i}"),
                [],
                |r| r.get(0),
            )?;
            let after: i64 =
                cell.conn
                    .query_row(&format!("SELECT count(*) FROM \"{quoted}\""), [], |r| {
                        r.get(0)
                    })?;
            diffs.push((t.clone(), before, after));
        }
    }

    cell.conn
        .execute_batch("ROLLBACK")
        .context("rolling back verify-replay transaction")?;

    if let Some((table, before, after)) = diffs.into_iter().next() {
        if before != after {
            anyhow::bail!(
                "replay-unsafe transform: re-running the pipeline against the same staged delta \
                 changed table '{table}' ({before} -> {after} rows). {VERIFY_REPLAY_TAIL}"
            );
        } else {
            anyhow::bail!(
                "replay-unsafe transform: re-running against the same staged delta changed the \
                 contents of table '{table}' (row count steady at {before}, content hash \
                 differs). {VERIFY_REPLAY_TAIL}"
            );
        }
    }
    Ok(())
}

/// Local relative source paths resolve against the cell directory (like transforms);
/// remote URIs and absolute paths pass through. Globs are preserved.
fn resolve_source_uri(uri: &str, dir: &Path) -> String {
    if crate::config::is_remote(uri) {
        return uri.to_string();
    }
    let p = uri.strip_prefix("file://").unwrap_or(uri);
    let path = Path::new(p);
    if path.is_absolute() {
        p.to_string()
    } else {
        dir.join(path).to_string_lossy().into_owned()
    }
}

pub(crate) fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

/// Resolve the storage URI. Local relative paths are made absolute against the
/// cell directory and created; remote URIs pass through untouched.
pub(crate) fn resolve_storage(s: &str, dir: &Path) -> Result<String> {
    if let Some(rest) = s.strip_prefix("file://") {
        return resolve_local(rest, dir, /* is_dir */ true);
    }
    if s.contains("://") {
        return Ok(s.to_string());
    }
    resolve_local(s, dir, true)
}

/// Resolve the catalog binding. `sqlite:` DSNs pass through as-is; `postgres://`
/// DSNs are translated to the libpq keyword string DuckLake actually expects
/// (see `postgres_url_to_ducklake`); anything else is a local catalog file path.
pub(crate) fn resolve_catalog(s: &str, dir: &Path) -> Result<String> {
    if s.starts_with("postgres://") {
        return postgres_url_to_ducklake(s);
    }
    if crate::config::is_metadata_db_catalog(s) {
        return Ok(s.to_string());
    }
    let p = s.strip_prefix("file://").unwrap_or(s);
    resolve_local(p, dir, /* is_dir */ false)
}

/// Translate `postgres://[user[:password]@]host[:port]/dbname` (the DSN form
/// `cell.yaml`/`profiles/*.yaml` document and every profile in this repo uses)
/// into `postgres:dbname=... host=... [port=...] [user=...] [password=...]` —
/// the libpq keyword/value connect string DuckLake's postgres catalog backend
/// actually parses after `ducklake:`.
///
/// Passing the URL form straight through (what this code did before it was
/// caught by the `kind` e2e harness, test/integrations/kind_e2e/README.md)
/// does NOT fail to parse — it fails silently *differently*: DuckDB doesn't
/// recognize `postgres://...` as a connection string at all and instead tries
/// to open a local file literally named `postgres:/user:pass@host/db`, "no
/// such file or directory". Translating at this boundary keeps the familiar
/// URL form in the contract without pushing DuckLake's connection-string
/// dialect into `cell.yaml`/profiles.
fn postgres_url_to_ducklake(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("postgres://")
        .ok_or_else(|| anyhow::anyhow!("not a postgres:// DSN: {url}"))?;

    let (userinfo, hostpart) = match rest.split_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, rest),
    };
    let (hostport, dbpart) = hostpart
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("postgres catalog DSN '{url}' has no `/<database>` path"))?;
    let dbname = dbpart.split('?').next().unwrap_or(dbpart);
    if dbname.is_empty() {
        anyhow::bail!("postgres catalog DSN '{url}' has an empty database name");
    }
    let (host, port) = match hostport.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (hostport, None),
    };
    if host.is_empty() {
        anyhow::bail!("postgres catalog DSN '{url}' has no host");
    }

    let mut parts = vec![format!("dbname={dbname}"), format!("host={host}")];
    if let Some(p) = port {
        parts.push(format!("port={p}"));
    }
    if let Some(u) = userinfo {
        match u.split_once(':') {
            Some((user, pass)) => {
                parts.push(format!("user={user}"));
                parts.push(format!("password={pass}"));
            }
            None => parts.push(format!("user={u}")),
        }
    }
    Ok(format!("postgres:{}", parts.join(" ")))
}

fn resolve_local(p: &str, dir: &Path, is_dir: bool) -> Result<String> {
    let path = Path::new(p);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        dir.join(path)
    };
    if is_dir {
        std::fs::create_dir_all(&abs)
            .with_context(|| format!("creating storage dir {}", abs.display()))?;
        // Canonicalize so the path stored in the catalog is clean (no `./`).
        let canon = std::fs::canonicalize(&abs)
            .with_context(|| format!("canonicalizing {}", abs.display()))?;
        Ok(canon.to_string_lossy().into_owned())
    } else if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating catalog dir {}", parent.display()))?;
        let canon = std::fs::canonicalize(parent)
            .with_context(|| format!("canonicalizing {}", parent.display()))?;
        Ok(canon
            .join(abs.file_name().unwrap())
            .to_string_lossy()
            .into_owned())
    } else {
        Ok(abs.to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esc_doubles_single_quotes() {
        assert_eq!(esc("plain"), "plain");
        assert_eq!(esc("o'brien"), "o''brien");
        assert_eq!(esc("a'b'c"), "a''b''c");
    }

    // Compaction selection (ADR 0004 §10): expire only past-retention
    // snapshots, never a pinned id, never the newest snapshot.

    #[test]
    fn select_expirable_takes_only_old_unpinned_snapshots() {
        // (id, unix_time); cutoff 100 -> 1 and 2 are old, 3 and 4 are fresh.
        let snaps = [(1, 50), (2, 60), (3, 150), (4, 160)];
        assert_eq!(select_expirable(&snaps, &[], 100), vec![1, 2]);
    }

    #[test]
    fn select_expirable_never_touches_a_pinned_snapshot() {
        let snaps = [(1, 50), (2, 60), (3, 70), (4, 160)];
        // 2 is pinned by a supported route: it must survive any window.
        assert_eq!(select_expirable(&snaps, &[2], 100), vec![1, 3]);
    }

    #[test]
    fn select_expirable_never_touches_the_newest_snapshot() {
        // Everything is past retention (a paused cell resuming after a long
        // gap) — the newest snapshot is the artifact's current state and must
        // survive.
        let snaps = [(1, 50), (2, 60), (3, 70)];
        assert_eq!(select_expirable(&snaps, &[], 1_000), vec![1, 2]);
    }

    #[test]
    fn select_expirable_is_empty_within_the_window() {
        let snaps = [(1, 150), (2, 160)];
        assert!(select_expirable(&snaps, &[], 100).is_empty());
        assert!(select_expirable(&[], &[], 100).is_empty());
    }

    // Found running the `kind` e2e harness (test/integrations/kind_e2e/):
    // DuckLake's postgres catalog backend takes a libpq keyword/value connect
    // string after `ducklake:postgres:`, not the `postgres://` URL every
    // profile in this repo (and the docs) use. `resolve_catalog` translates at
    // the boundary; these pin the translation down without a live Postgres.

    #[test]
    fn postgres_url_translates_user_password_host_port_db() {
        assert_eq!(
            postgres_url_to_ducklake("postgres://datamk:datamk@db.example:5432/orders").unwrap(),
            "postgres:dbname=orders host=db.example port=5432 user=datamk password=datamk"
        );
    }

    #[test]
    fn postgres_url_handles_missing_port_and_password() {
        assert_eq!(
            postgres_url_to_ducklake("postgres://u@h/db").unwrap(),
            "postgres:dbname=db host=h user=u"
        );
    }

    #[test]
    fn postgres_url_handles_no_userinfo_at_all() {
        assert_eq!(
            postgres_url_to_ducklake("postgres://db.example:5432/orders").unwrap(),
            "postgres:dbname=orders host=db.example port=5432"
        );
    }

    #[test]
    fn postgres_url_rejects_a_missing_database() {
        let err = postgres_url_to_ducklake("postgres://h/")
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty database name"), "got: {err}");
    }

    #[test]
    fn postgres_url_rejects_a_missing_database_path() {
        let err = postgres_url_to_ducklake("postgres://h")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no `/<database>` path"), "got: {err}");
    }

    #[test]
    fn resolve_catalog_translates_postgres_urls() {
        let dir = Path::new("/cell");
        assert_eq!(
            resolve_catalog("postgres://datamk:datamk@db:5432/orders", dir).unwrap(),
            "postgres:dbname=orders host=db port=5432 user=datamk password=datamk"
        );
    }

    #[test]
    fn resolve_catalog_passes_sqlite_dsns_through_untranslated() {
        let dir = Path::new("/cell");
        assert_eq!(
            resolve_catalog("sqlite:/data/catalog.db", dir).unwrap(),
            "sqlite:/data/catalog.db"
        );
    }

    #[test]
    fn resolve_source_uri_passes_remote_through_untouched() {
        let dir = Path::new("/cell");
        assert_eq!(
            resolve_source_uri("s3://b/x.parquet", dir),
            "s3://b/x.parquet"
        );
        assert_eq!(
            resolve_source_uri("gs://b/*.parquet", dir),
            "gs://b/*.parquet"
        );
    }

    fn bindings_for_secrets(
        storage: &str,
        source: Option<&str>,
        s3: Option<ResolvedS3>,
        gcs: Option<ResolvedGcs>,
    ) -> ResolvedBindings {
        let mut sources = IndexMap::new();
        if let Some(uri) = source {
            sources.insert("src".to_string(), ResolvedSource::Raw(uri.to_string()));
        }
        ResolvedBindings {
            catalog: None,
            storage: storage.to_string(),
            s3,
            gcs,
            sources,
            principals: None,
        }
    }

    #[test]
    fn store_secrets_branch_by_scheme_not_by_uses_remote() {
        // The regression this guards: a gs://-only cell used to get a TYPE s3
        // secret (and no gcs one) because the gate only knew "remote".
        let gs_only = bindings_for_secrets("gs://bkt/c", None, None, None);
        assert_eq!(store_secrets_needed("gs://bkt/c", &gs_only), (false, true));

        let s3_only = bindings_for_secrets("s3://bkt/c", None, None, None);
        assert_eq!(store_secrets_needed("s3://bkt/c", &s3_only), (true, false));

        // Mixed cell: s3:// source beside gs:// storage needs both secrets.
        let mixed = bindings_for_secrets("gs://bkt/c", Some("s3://other/x.parquet"), None, None);
        assert_eq!(store_secrets_needed("gs://bkt/c", &mixed), (true, true));

        let local = bindings_for_secrets("./.cell/data", None, None, None);
        assert_eq!(store_secrets_needed("./.cell/data", &local), (false, false));

        // A gcs block with only `credentials:` (store plane) must not force a
        // DuckDB secret — that path would fail loud on the missing HMAC pair.
        let store_only_gcs = bindings_for_secrets(
            "./.cell/data",
            None,
            None,
            Some(ResolvedGcs {
                credentials: Some("/k.json".to_string()),
                extension: None,
                key_id: None,
                secret: None,
                endpoint: None,
                use_ssl: None,
            }),
        );
        assert_eq!(
            store_secrets_needed("./.cell/data", &store_only_gcs),
            (false, false)
        );
    }

    #[test]
    fn gcs_secret_options_requires_the_hmac_pair() {
        let err = gcs_secret_options(None).unwrap_err().to_string();
        assert!(err.contains("gcs.key_id"), "unexpected error: {err}");
        assert!(
            err.contains("gcloud storage hmac create"),
            "unexpected error: {err}"
        );

        // `credentials:` alone configures the native store client, not DuckDB.
        let store_only = ResolvedGcs {
            credentials: Some("/etc/datamk/gcs-key.json".to_string()),
            extension: None,
            key_id: None,
            secret: None,
            endpoint: None,
            use_ssl: None,
        };
        assert!(gcs_secret_options(Some(&store_only)).is_err());
    }

    #[test]
    fn gcs_secret_options_native_extension_is_keyless() {
        // Extension set, no HMAC pair: TYPE GCP on the ambient ADC chain.
        let adc = ResolvedGcs {
            credentials: None,
            extension: Some("/opt/datamk/gcs.duckdb_extension".to_string()),
            key_id: None,
            secret: None,
            endpoint: None,
            use_ssl: None,
        };
        assert_eq!(gcs_secret_options(Some(&adc)).unwrap(), "TYPE GCP");
        assert_eq!(
            gcs_load_sql(Some(&adc)).as_deref(),
            Some("LOAD '/opt/datamk/gcs.duckdb_extension';")
        );

        // With `credentials`, the same key file drives DuckDB and the store.
        let sa = ResolvedGcs {
            credentials: Some("/etc/datamk/gcs-key.json".to_string()),
            ..adc.clone()
        };
        assert_eq!(
            gcs_secret_options(Some(&sa)).unwrap(),
            "TYPE GCP, SERVICE_ACCOUNT_KEY_PATH '/etc/datamk/gcs-key.json'"
        );

        // An extension-bearing block also counts as "DuckDB can reach gs://".
        let b = bindings_for_secrets("./.cell/data", None, None, Some(adc));
        assert_eq!(store_secrets_needed("./.cell/data", &b), (false, true));
    }

    #[test]
    fn gcs_secret_options_emits_hmac_and_emulator_endpoint() {
        let gcs = ResolvedGcs {
            credentials: None,
            extension: None,
            key_id: Some("HMACKEY".to_string()),
            secret: Some("it's secret".to_string()),
            endpoint: Some("fake-gcs:4443".to_string()),
            use_ssl: Some(false),
        };
        assert_eq!(
            gcs_secret_options(Some(&gcs)).unwrap(),
            "TYPE gcs, KEY_ID 'HMACKEY', SECRET 'it''s secret', ENDPOINT 'fake-gcs:4443', \
             USE_SSL false"
        );
    }

    #[test]
    fn resolve_source_uri_keeps_absolute_local_paths() {
        let dir = Path::new("/cell");
        assert_eq!(
            resolve_source_uri("/data/x.parquet", dir),
            "/data/x.parquet"
        );
        // file:// prefix is stripped before checking absoluteness.
        assert_eq!(
            resolve_source_uri("file:///data/x.parquet", dir),
            "/data/x.parquet"
        );
    }

    #[test]
    fn resolve_source_uri_joins_relative_paths_to_cell_dir() {
        let dir = Path::new("/cell");
        assert_eq!(
            resolve_source_uri("data/x.parquet", dir),
            "/cell/data/x.parquet"
        );
        // Globs are preserved through the join.
        assert_eq!(
            resolve_source_uri("data/*.parquet", dir),
            "/cell/data/*.parquet"
        );
    }

    // --- ADR 0005: incremental source loading -----------------------------
    //
    // Unit tests for the seam described in the plan: pure functions (literal
    // rendering, cursor/lookback validation, the shrink comparator) need no
    // connection at all; the staging/watermark/replay mechanics are driven
    // against a locally ATTACHed DuckLake catalog, standing in for "the
    // warehouse" as a plain table in `lake` (no live connector needed —
    // `stage_incremental` only cares about a qualified table expression and
    // a connection, never how it got attached).

    #[test]
    fn mark_value_renders_typed_literals() {
        assert_eq!(
            MarkValue::Ts("2026-07-04 10:58:00+05:30".to_string()).as_literal(),
            "TIMESTAMPTZ '2026-07-04 10:58:00+05:30'"
        );
        assert_eq!(
            MarkValue::Date("2026-07-04".to_string()).as_literal(),
            "DATE '2026-07-04'"
        );
        assert_eq!(MarkValue::Int(42).as_literal(), "42");
    }

    #[test]
    fn mark_value_literal_cannot_be_escaped_by_a_quote_in_the_value() {
        // Defense in depth: the literal is built by string formatting, not a
        // bound parameter, so a value containing a quote must not be able to
        // break out of it.
        let hostile = MarkValue::Ts("2026-01-01'; DROP TABLE t; --".to_string());
        assert_eq!(
            hostile.as_literal(),
            "TIMESTAMPTZ '2026-01-01''; DROP TABLE t; --'"
        );
        let hostile_date = MarkValue::Date("2026-01-01' OR '1'='1".to_string());
        assert_eq!(
            hostile_date.as_literal(),
            "DATE '2026-01-01'' OR ''1''=''1'"
        );
    }

    #[test]
    fn classify_cursor_type_covers_the_closed_set() {
        assert_eq!(
            classify_cursor_type("TIMESTAMP"),
            Some(CursorType::Timestamp)
        );
        assert_eq!(
            classify_cursor_type("TIMESTAMP WITH TIME ZONE"),
            Some(CursorType::Timestamp)
        );
        assert_eq!(
            classify_cursor_type("TIMESTAMP_NS"),
            Some(CursorType::Timestamp)
        );
        assert_eq!(classify_cursor_type("DATE"), Some(CursorType::Date));
        for int_ty in [
            "TINYINT", "SMALLINT", "INTEGER", "BIGINT", "HUGEINT", "UBIGINT",
        ] {
            assert_eq!(classify_cursor_type(int_ty), Some(CursorType::Integer));
        }
        assert_eq!(classify_cursor_type("VARCHAR"), None);
        assert_eq!(classify_cursor_type("DOUBLE"), None);
    }

    #[test]
    fn validate_cursor_reports_missing_column_with_available_list() {
        let cols = vec![
            ("id".to_string(), "INTEGER".to_string(), false),
            ("region".to_string(), "VARCHAR".to_string(), false),
        ];
        let err = validate_cursor("events", "analytics.events", "updated_at", &cols)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("incremental cursor 'updated_at' does not exist in analytics.events"),
            "got: {err}"
        );
        assert!(err.contains("Columns available: id, region"), "got: {err}");
        assert!(err.contains("drop `incremental:`"), "got: {err}");
    }

    #[test]
    fn validate_cursor_rejects_an_unsupported_type() {
        let cols = vec![("updated_at".to_string(), "VARCHAR".to_string(), false)];
        let err = validate_cursor("events", "analytics.events", "updated_at", &cols)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("incremental cursor 'updated_at' is VARCHAR"),
            "got: {err}"
        );
        assert!(err.contains("timestamp, date, or integer"), "got: {err}");
    }

    #[test]
    fn validate_cursor_matches_case_insensitively_and_classifies() {
        let cols = vec![("Updated_At".to_string(), "TIMESTAMP".to_string(), true)];
        let (ty, actual_ty, nullable) =
            validate_cursor("events", "t", "updated_at", &cols).unwrap();
        assert_eq!(ty, CursorType::Timestamp);
        assert_eq!(actual_ty, "TIMESTAMP");
        assert!(nullable);
    }

    #[test]
    fn format_duration_reconstructs_a_canonical_suffix() {
        assert_eq!(format_duration(Duration::from_secs(30)), "30s");
        assert_eq!(format_duration(Duration::from_secs(120)), "2m");
        assert_eq!(format_duration(Duration::from_secs(7200)), "2h");
        assert_eq!(format_duration(Duration::from_secs(172_800)), "2d");
    }

    #[test]
    fn lookback_is_fine_on_a_timestamp_cursor() {
        let sql = validate_lookback(
            "events",
            "updated_at",
            CursorType::Timestamp,
            "TIMESTAMP",
            Some(Duration::from_secs(7200)),
        )
        .unwrap();
        assert_eq!(sql.as_deref(), Some("INTERVAL '7200' SECOND"));
    }

    #[test]
    fn lookback_is_none_when_not_configured() {
        let sql = validate_lookback("events", "id", CursorType::Integer, "BIGINT", None).unwrap();
        assert!(sql.is_none());
    }

    #[test]
    fn lookback_is_rejected_on_an_integer_cursor() {
        let err = validate_lookback(
            "events",
            "id",
            CursorType::Integer,
            "BIGINT",
            Some(Duration::from_secs(7200)),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("`lookback: 2h` needs a time-typed cursor"),
            "got: {err}"
        );
        assert!(err.contains("is BIGINT"), "got: {err}");
        assert!(
            err.contains("integer cursors have no time window"),
            "got: {err}"
        );
    }

    #[test]
    fn lookback_is_rejected_on_a_date_cursor_unless_whole_days() {
        let err = validate_lookback(
            "events",
            "d",
            CursorType::Date,
            "DATE",
            Some(Duration::from_secs(3_600)),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("whole number of days"), "got: {err}");

        let sql = validate_lookback(
            "events",
            "d",
            CursorType::Date,
            "DATE",
            Some(Duration::from_secs(172_800)),
        )
        .unwrap();
        assert_eq!(sql.as_deref(), Some("INTERVAL '2' DAY"));
    }

    #[test]
    fn shrunk_tables_reports_only_tables_present_in_both_snapshots_that_decreased() {
        let mut before = IndexMap::new();
        before.insert("t".to_string(), 100);
        before.insert("u".to_string(), 5);
        let mut after = IndexMap::new();
        after.insert("t".to_string(), 3); // shrank
        after.insert("u".to_string(), 5); // unchanged
        after.insert("v".to_string(), 1); // new table, not in `before` -> ignored
        assert_eq!(
            shrunk_tables(&before, &after),
            vec![("t".to_string(), 100, 3)]
        );
    }

    #[test]
    fn shrunk_tables_is_empty_when_nothing_shrank() {
        let mut before = IndexMap::new();
        before.insert("t".to_string(), 10);
        let mut after = IndexMap::new();
        after.insert("t".to_string(), 10);
        assert!(shrunk_tables(&before, &after).is_empty());
    }

    #[test]
    fn shrink_summary_lines_name_every_table_before_and_after_with_the_docs_hint() {
        // Deliberately mirrors the ADR 0008 incident narrative's own numbers
        // (351k accumulated rows collapsed to a 45-row delta) — thousands-
        // grouped via the same `group_thousands` `status`/`rollback` use, so
        // a big-number regression is a big, readable number, not "351000".
        let shrunk = vec![
            ("fct_orders".to_string(), 351_000, 45),
            ("dim_customers".to_string(), 200, 199),
        ];
        let lines = format_shrink_summary_lines(&shrunk);
        assert_eq!(
            lines,
            vec![
                "".to_string(),
                "WARNING: 2 tables shrank during this run:".to_string(),
                "  fct_orders: 351,000 -> 45 rows".to_string(),
                "  dim_customers: 200 -> 199 rows".to_string(),
                "  If any of these tables accumulate from an incremental source, the \
                 transform likely replaced history with the delta instead of merging into it \
                 — see docs/guides/incremental.md"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn shrink_summary_lines_singular_table_wording() {
        let shrunk = vec![("t".to_string(), 10, 3)];
        let lines = format_shrink_summary_lines(&shrunk);
        assert_eq!(lines[1], "WARNING: 1 table shrank during this run:");
    }

    // --- --full-refresh's three states (ADR 0005 §3, ADR 0008 §6) ----------

    #[test]
    fn full_refresh_effect_prefers_incremental_sources_when_present() {
        // Both incremental sources and declarative tables present: the
        // incremental-sources message takes priority (it already covers
        // "watermarks rewind", the more expensive/surprising half of what
        // --full-refresh does).
        assert_eq!(
            full_refresh_effect(2, 3),
            FullRefreshEffect::IncrementalSources(2)
        );
        assert_eq!(
            full_refresh_effect(1, 0),
            FullRefreshEffect::IncrementalSources(1)
        );
    }

    #[test]
    fn full_refresh_effect_is_declarative_only_with_no_incremental_sources() {
        assert_eq!(
            full_refresh_effect(0, 3),
            FullRefreshEffect::DeclarativeOnly(3)
        );
        assert_eq!(
            full_refresh_effect(0, 1),
            FullRefreshEffect::DeclarativeOnly(1)
        );
    }

    #[test]
    fn full_refresh_effect_is_no_effect_with_neither() {
        assert_eq!(full_refresh_effect(0, 0), FullRefreshEffect::NoEffect);
    }

    /// R5: no new connection type — this drives the incremental seam
    /// (`stage_incremental`/`persist_watermarks`) against a plain table
    /// created directly in the locally ATTACHed `lake`, standing in for "the
    /// warehouse" the way a connector's `qualify()` output would.
    fn seed_warehouse(conn: &Connection, rows: &[(i64, &str)]) {
        conn.execute_batch("CREATE TABLE warehouse (id INTEGER, updated_at TIMESTAMP);")
            .unwrap();
        for (id, ts) in rows {
            conn.execute(
                &format!("INSERT INTO warehouse VALUES ({id}, TIMESTAMP '{ts}');"),
                [],
            )
            .unwrap();
        }
    }

    fn ts_incremental() -> ResolvedIncremental {
        ResolvedIncremental {
            cursor: "updated_at".to_string(),
            lookback: None,
        }
    }

    /// A `ResolvedConnection`/`ObjectMeta` pair standing in for a classified
    /// BASE TABLE — `stage_incremental`'s `ObjectKind::Table` arm ignores
    /// both beyond `meta.kind`, so these are never read for the local-table
    /// tests below (R5: no live warehouse needed).
    fn dummy_bq() -> ResolvedConnection {
        ResolvedConnection::Bigquery {
            project: "p".to_string(),
            billing_project: None,
            credentials: None,
            staging_uri: None,
        }
    }

    fn table_meta() -> ObjectMeta {
        ObjectMeta {
            kind: ObjectKind::Table,
            columns: IndexMap::new(),
        }
    }

    #[test]
    fn stage_incremental_bootstraps_then_stages_only_the_delta_on_the_next_run() {
        let (conn, _dir) = probe_attach("stage-two-run");
        seed_warehouse(
            &conn,
            &[(1, "2026-01-01 00:00:00"), (2, "2026-01-02 00:00:00")],
        );
        let inc = ts_incremental();
        let (config, meta) = (dummy_bq(), table_meta());

        // Run 1: no watermark yet -> bootstrap, unfiltered, both rows.
        let adv1 = stage_incremental(
            &conn,
            0,
            "events",
            "events",
            "analytics.events",
            "\"warehouse\"",
            "warehouse",
            &config,
            &meta,
            &inc,
            false,
            "t",
            None,
            None,
        )
        .unwrap();
        assert_eq!(adv1.staged_rows, 2);
        assert!(matches!(&adv1.new_max, Some(MarkValue::Ts(s)) if s.starts_with("2026-01-02")));
        persist_watermarks(&conn, std::slice::from_ref(&adv1), false).unwrap();

        // Upstream grows.
        conn.execute(
            "INSERT INTO warehouse VALUES (3, TIMESTAMP '2026-01-03 00:00:00');",
            [],
        )
        .unwrap();

        // Run 2: only the new row stages.
        let adv2 = stage_incremental(
            &conn,
            1,
            "events",
            "events",
            "analytics.events",
            "\"warehouse\"",
            "warehouse",
            &config,
            &meta,
            &inc,
            false,
            "t",
            None,
            None,
        )
        .unwrap();
        assert_eq!(adv2.staged_rows, 1);
        assert!(matches!(&adv2.new_max, Some(MarkValue::Ts(s)) if s.starts_with("2026-01-03")));
    }

    #[test]
    fn stage_incremental_full_refresh_ignores_the_watermark_and_reads_everything() {
        let (conn, _dir) = probe_attach("stage-full-refresh");
        seed_warehouse(
            &conn,
            &[(1, "2026-01-01 00:00:00"), (2, "2026-01-02 00:00:00")],
        );
        let inc = ts_incremental();
        let (config, meta) = (dummy_bq(), table_meta());
        let adv1 = stage_incremental(
            &conn,
            0,
            "events",
            "events",
            "analytics.events",
            "\"warehouse\"",
            "warehouse",
            &config,
            &meta,
            &inc,
            false,
            "t",
            None,
            None,
        )
        .unwrap();
        persist_watermarks(&conn, std::slice::from_ref(&adv1), false).unwrap();

        // A plain run now sees nothing new.
        let adv2 = stage_incremental(
            &conn,
            1,
            "events",
            "events",
            "analytics.events",
            "\"warehouse\"",
            "warehouse",
            &config,
            &meta,
            &inc,
            false,
            "t",
            None,
            None,
        )
        .unwrap();
        assert_eq!(adv2.staged_rows, 0);

        // --full-refresh re-reads everything regardless of the watermark.
        let adv3 = stage_incremental(
            &conn,
            2,
            "events",
            "events",
            "analytics.events",
            "\"warehouse\"",
            "warehouse",
            &config,
            &meta,
            &inc,
            true,
            "t",
            None,
            None,
        )
        .unwrap();
        assert_eq!(adv3.staged_rows, 2);
    }

    #[test]
    fn empty_delta_never_writes_a_watermark_row_or_table() {
        let (conn, _dir) = probe_attach("empty-delta");
        conn.execute_batch("CREATE TABLE warehouse (id INTEGER, updated_at TIMESTAMP);")
            .unwrap();
        let inc = ts_incremental();
        let (config, meta) = (dummy_bq(), table_meta());
        let adv = stage_incremental(
            &conn,
            0,
            "events",
            "events",
            "analytics.events",
            "\"warehouse\"",
            "warehouse",
            &config,
            &meta,
            &inc,
            false,
            "t",
            None,
            None,
        )
        .unwrap();
        assert_eq!(adv.staged_rows, 0);
        assert!(adv.new_max.is_none());

        // The NULL-greatest pin: persisting an empty-delta advance must not
        // even create the watermark table, let alone attempt
        // `greatest(old, NULL)`.
        persist_watermarks(&conn, &[adv], false).unwrap();
        assert!(!watermark_table_exists(&conn).unwrap());
    }

    #[test]
    fn upsert_advances_with_greatest_but_full_refresh_overwrites() {
        let (conn, _dir) = probe_attach("upsert-greatest");
        seed_warehouse(&conn, &[(1, "2026-01-05 00:00:00")]);
        let inc = ts_incremental();
        let (config, meta) = (dummy_bq(), table_meta());
        let adv1 = stage_incremental(
            &conn,
            0,
            "events",
            "events",
            "analytics.events",
            "\"warehouse\"",
            "warehouse",
            &config,
            &meta,
            &inc,
            false,
            "t",
            None,
            None,
        )
        .unwrap();
        persist_watermarks(&conn, std::slice::from_ref(&adv1), false).unwrap();

        let older = WatermarkAdvance {
            source: "events".to_string(),
            cursor: "updated_at".to_string(),
            ty: CursorType::Timestamp,
            new_max: Some(MarkValue::Ts("2026-01-01 00:00:00+00".to_string())),
            staged_rows: 1,
        };
        persist_watermarks(&conn, std::slice::from_ref(&older), false).unwrap();
        let mark: String = conn
            .query_row(
                "SELECT mark_ts::VARCHAR FROM __datamk_watermarks WHERE source = 'events'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            mark.starts_with("2026-01-05"),
            "watermark must not move backwards, got {mark}"
        );

        persist_watermarks(&conn, std::slice::from_ref(&older), true).unwrap();
        let mark2: String = conn
            .query_row(
                "SELECT mark_ts::VARCHAR FROM __datamk_watermarks WHERE source = 'events'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            mark2.starts_with("2026-01-01"),
            "--full-refresh must overwrite the mark, got {mark2}"
        );
    }

    #[test]
    fn duplicate_watermark_rows_fail_loud() {
        let (conn, _dir) = probe_attach("dup-watermark");
        conn.execute_batch(
            "CREATE TABLE __datamk_watermarks ( \
               source VARCHAR NOT NULL, cursor_column VARCHAR NOT NULL, \
               mark_ts TIMESTAMPTZ, mark_date DATE, mark_int BIGINT, last_delta_rows BIGINT); \
             INSERT INTO __datamk_watermarks VALUES \
               ('events', 'updated_at', TIMESTAMPTZ '2026-01-01 00:00:00+00', NULL, NULL, 5), \
               ('events', 'updated_at', TIMESTAMPTZ '2026-01-02 00:00:00+00', NULL, NULL, 3);",
        )
        .unwrap();
        let err = check_watermark_duplicates(&conn).unwrap_err().to_string();
        assert!(err.contains("duplicate rows"), "got: {err}");
        assert!(err.contains("events"), "got: {err}");
    }

    // `replay_test_cell` (a raw-DML test-cell builder) is gone along with
    // the raw path it built for — `materialize_test_cell` + `run_transforms`
    // (below) is the one remaining way to build a committed test cell, for
    // every kind of transform now that there is only one kind (ADR 0008).
    //
    // Two of the four tests that used to live here are gone outright, not
    // converted, because the failure mode they proved verify_replay catches
    // is now categorically unwritable through the declarative surface —
    // exactly ADR 0008's own claim ("the truncation failure is unwritable
    // in a transform file"), now true of duplication and non-re-runnability
    // too:
    //   - A hand-written duplicating `INSERT ... SELECT` (the old
    //     `verify_replay_fails_on_a_plain_insert_that_duplicates`) cannot be
    //     authored anymore: `upsert`/`append` reconcile against existing
    //     state by construction, and there is no other way to land a row.
    //   - A non-re-runnable statement (the old
    //     `verify_replay_wraps_a_non_rerunnable_transform_with_a_named_cause`,
    //     a bare `CREATE TABLE` with neither `OR REPLACE` nor
    //     `IF NOT EXISTS`) cannot be authored either: every statement the
    //     engine composes for every strategy already uses one or the other.
    //     The `verify_replay` code path that wraps a re-run failure with
    //     "must be re-runnable" (`with_context` in `verify_replay` above)
    //     is therefore believed unreachable through any declarative entry
    //     today — kept as defensive plumbing (a real filesystem race, a
    //     future strategy bug), not because a test still exercises it. Flagged
    //     here rather than silently dropped.

    #[test]
    fn verify_replay_passes_on_an_append_transform() {
        // The anti-join shape the old hand-written test built by hand is
        // exactly what `materialize: append` composes natively now.
        let cell = materialize_test_cell(
            "replay-append",
            "  - sql: sql/fct.sql\n    materialize: append\n    key: [id]\n",
            &[("fct.sql", "SELECT id FROM range(5) r(id)")],
        );
        run_transforms(&cell, false).unwrap();
        verify_replay(&cell, false).expect("append must be replay-safe through verify_replay");
    }

    #[test]
    fn verify_replay_passes_on_a_replace_rebuild() {
        // Truncation-shaped rebuilds are idempotent and structurally
        // invisible to a pass1-vs-pass2 comparison (ADR 0005 §2) —
        // --verify-replay must NOT flag a `replace` entry; the shrink
        // warning is the detector for this shape instead. Every `replace`
        // entry has exactly this shape now (ADR 0008) — there is no other
        // kind of "CREATE OR REPLACE" transform left to construct.
        let cell = materialize_test_cell(
            "replay-replace",
            "  - sql/fct.sql\n", // bare path = materialize: replace (the default)
            &[("fct.sql", "SELECT id FROM range(5) r(id)")],
        );
        run_transforms(&cell, false).unwrap();
        verify_replay(&cell, false).expect("a replace rebuild must not fail verify-replay");
    }

    // --- ADR 0005 (--verify-replay) probes ------------------------------
    //
    // These are exploratory probes (not yet promoted) run to answer two
    // empirical questions the verify-replay design depends on:
    //   1. Is a DuckLake transaction ROLLBACK clean, including DDL
    //      (CREATE OR REPLACE TABLE, CREATE TABLE IF NOT EXISTS, INSERT)?
    //   2. Do read-only statements (SELECT, DESCRIBE, information_schema)
    //      create new DuckLake snapshots?
    // Both open a fresh in-memory DuckDB + fresh DuckLake catalog in a tempdir
    // (mirrors `new_scratch_dir`/`setup` above but standalone, no `CellDef`).

    fn probe_scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datamk-probe-{tag}-{}-{}",
            std::process::id(),
            SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn probe_attach(tag: &str) -> (Connection, PathBuf) {
        let dir = probe_scratch_dir(tag);
        let conn = Connection::open_in_memory().expect("open in-memory duckdb");
        conn.execute_batch("INSTALL ducklake; LOAD ducklake; INSTALL json; LOAD json;")
            .expect("install/load ducklake");
        let catalog = dir.join("probe.ducklake");
        let data = dir.join("data");
        conn.execute_batch(&format!(
            "ATTACH 'ducklake:{}' AS lake (DATA_PATH '{}'); USE lake;",
            catalog.to_string_lossy(),
            data.to_string_lossy()
        ))
        .expect("attach ducklake");
        (conn, dir)
    }

    fn max_snapshot_id(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT max(snapshot_id) FROM ducklake_snapshots('lake')",
            [],
            |r| r.get(0),
        )
        .expect("query max snapshot id")
    }

    #[test]
    fn ducklake_rollback_restores_committed_state_and_snapshots() {
        // Pins the semantics `--verify-replay` (ADR 0005) depends on: a
        // transaction (including DDL) that is ROLLBACK'd leaves both table
        // contents and the snapshot history exactly as they were before BEGIN.
        let (conn, _dir) = probe_attach("rollback");

        conn.execute_batch("CREATE TABLE t AS SELECT * FROM range(100) tbl(id);")
            .expect("create baseline table t");
        let baseline_snapshot = max_snapshot_id(&conn);

        conn.execute_batch("BEGIN;").expect("begin");
        conn.execute_batch("CREATE OR REPLACE TABLE t AS SELECT * FROM range(5) tbl(id);")
            .expect("create or replace t inside txn");
        conn.execute_batch("CREATE TABLE IF NOT EXISTS t2(x INT);")
            .expect("create t2 inside txn");
        conn.execute_batch("INSERT INTO t VALUES (999);")
            .expect("insert into t inside txn");

        // Read-your-own-writes probe: uncommitted CREATE OR REPLACE must be
        // visible to a SELECT inside the same transaction.
        conn.execute_batch("CREATE OR REPLACE TABLE t3 AS SELECT * FROM range(7) tbl(id);")
            .expect("create t3 inside txn");
        let t3_count_in_txn: i64 = conn
            .query_row("SELECT count(*) FROM t3", [], |r| r.get(0))
            .expect("select t3 inside same txn (read-your-own-writes)");
        assert_eq!(
            t3_count_in_txn, 7,
            "expected to read uncommitted CREATE OR REPLACE within the same transaction"
        );

        conn.execute_batch("ROLLBACK;").expect("rollback");

        let t_count: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .expect("select count(*) from t after rollback");
        assert_eq!(
            t_count, 100,
            "t should be restored to its committed 100 rows"
        );

        let t2_exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM information_schema.tables \
                 WHERE table_catalog = 'lake' AND table_name = 't2'",
                [],
                |r| r.get(0),
            )
            .expect("check t2 existence");
        assert_eq!(t2_exists, 0, "t2 must not exist after rollback");

        let after_snapshot = max_snapshot_id(&conn);
        assert_eq!(
            after_snapshot, baseline_snapshot,
            "rollback must not create a new snapshot"
        );
    }

    #[test]
    fn ducklake_readonly_statements_create_no_snapshots() {
        // Pins the semantics `--verify-replay` (ADR 0005) depends on:
        // auto-commit read-only statements (SELECT / DESCRIBE /
        // information_schema) never advance the DuckLake snapshot history.
        let (conn, _dir) = probe_attach("readonly");

        conn.execute_batch("CREATE TABLE t AS SELECT * FROM range(100) tbl(id);")
            .expect("create baseline table t");
        let baseline_snapshot = max_snapshot_id(&conn);

        let _: i64 = conn
            .query_row(
                "SELECT count(*) FROM information_schema.tables \
                 WHERE table_catalog = 'lake' AND table_name = 'no_such_table'",
                [],
                |r| r.get(0),
            )
            .expect("information_schema query with table_catalog = 'lake'");

        conn.execute_batch("DESCRIBE t;")
            .expect("describe t (auto-commit)");

        let _: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .expect("plain select over t (auto-commit)");

        let after_snapshot = max_snapshot_id(&conn);
        assert_eq!(
            after_snapshot, baseline_snapshot,
            "read-only statements must not create a new snapshot"
        );

        // Report the exact table_catalog value rows for `t` actually carry,
        // and whether a `table_catalog = 'lake'` filter finds it at all.
        let mut stmt = conn
            .prepare(
                "SELECT table_catalog, table_schema, table_name \
                 FROM information_schema.tables WHERE table_name = 't'",
            )
            .expect("prepare information_schema.tables lookup for t");
        let rows: Vec<(String, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .expect("query_map information_schema.tables")
            .collect::<std::result::Result<_, _>>()
            .expect("collect information_schema.tables rows");
        eprintln!("information_schema.tables rows for 't': {rows:?}");
        assert!(!rows.is_empty(), "expected at least one row for table t");

        let catalog_lake_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM information_schema.tables \
                 WHERE table_catalog = 'lake' AND table_name = 't'",
                [],
                |r| r.get(0),
            )
            .expect("count with table_catalog = 'lake'");
        eprintln!("rows matching table_catalog = 'lake' for 't': {catalog_lake_count}");
    }

    // --- ADR 0008 (declarative incremental materialization) MERGE gate ---
    //
    // §5b names an explicit pre-adoption gate: MERGE has never been
    // exercised through --verify-replay's snapshot-and-rollback path or
    // through a plain SQL ROLLBACK, and the probe that proved MERGE works in
    // isolation found **no existing integration test** exercising
    // DELETE/UPDATE/MERGE against an attached DuckLake table at all. These
    // tests close that gap before MERGE is wired into the `upsert` run path
    // (ADR 0008 Work items #3).

    /// Row multiset helper: `(id, val)` pairs from `table`, sorted by `id` so
    /// assert_eq! compares content, not incidental physical/insertion order.
    fn select_id_val(conn: &Connection, table: &str) -> Vec<(i32, String)> {
        let mut stmt = conn
            .prepare(&format!("SELECT id, val FROM \"{table}\" ORDER BY id"))
            .expect("prepare select id, val");
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query_map id, val")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect id, val rows")
    }

    #[test]
    fn merge_in_transaction_against_attached_ducklake_table() {
        // Gate item 1: MERGE composed the way the `upsert` strategy would
        // emit it, wrapped in the same BEGIN...COMMIT the Builder uses
        // (`run`, ~594-620), against a table in a DuckLake catalog attached
        // exactly as `setup` attaches it (`probe_attach` mirrors
        // `setup`'s ATTACH sequence, ~171-225).
        let (conn, _dir) = probe_attach("merge-txn");
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER, val VARCHAR); \
             INSERT INTO t VALUES (1, 'a'), (2, 'b');",
        )
        .expect("seed baseline table t");

        conn.execute_batch("BEGIN").expect("begin");
        conn.execute_batch(
            "MERGE INTO t USING (VALUES (2, 'b2'), (3, 'c')) AS s(id, val) \
             ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET val = s.val \
             WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val);",
        )
        .expect("MERGE INTO inside a transaction");
        conn.execute_batch("COMMIT").expect("commit");

        assert_eq!(
            select_id_val(&conn, "t"),
            vec![
                (1, "a".to_string()),
                (2, "b2".to_string()),
                (3, "c".to_string()),
            ],
            "MERGE must update matched key 2, insert unmatched key 3, and leave key 1 untouched"
        );
    }

    #[test]
    fn merge_using_temp_view_source_the_staged_delta_shape() {
        // Gate item 2: ADR 0008 §4 stages the SELECT into a TEMP relation
        // once, then MERGEs against that relation rather than re-evaluating
        // the SELECT per reference. This is that USING shape — a TEMP VIEW,
        // not an inline VALUES list.
        let (conn, _dir) = probe_attach("merge-tempview");
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER, val VARCHAR); \
             INSERT INTO t VALUES (1, 'a'), (2, 'b');",
        )
        .expect("seed baseline table t");
        conn.execute_batch(
            "CREATE TEMP VIEW delta AS SELECT * FROM (VALUES (2, 'b2'), (3, 'c')) AS s(id, val);",
        )
        .expect("create staged-delta TEMP VIEW");

        conn.execute_batch("BEGIN").expect("begin");
        conn.execute_batch(
            "MERGE INTO t USING delta AS s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET val = s.val \
             WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val);",
        )
        .expect("MERGE INTO USING a TEMP VIEW source");
        conn.execute_batch("COMMIT").expect("commit");

        assert_eq!(
            select_id_val(&conn, "t"),
            vec![
                (1, "a".to_string()),
                (2, "b2".to_string()),
                (3, "c".to_string()),
            ],
            "MERGE ... USING <TEMP VIEW> must behave identically to USING an inline VALUES list"
        );
    }

    #[test]
    fn merge_is_idempotent_across_two_committed_transactions() {
        // Gate item 3: at-least-once redelivery of the identical delta
        // across two separate, fully committed transactions must leave the
        // table unchanged the second time — the replay-safety property
        // ADR 0008 §3 leans on for `upsert`.
        let (conn, _dir) = probe_attach("merge-idempotent");
        conn.execute_batch("CREATE TABLE t (id INTEGER, val VARCHAR);")
            .expect("create baseline table t");

        let merge_sql = "MERGE INTO t USING (VALUES (1, 'a'), (2, 'b')) AS s(id, val) \
             ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET val = s.val \
             WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val);";

        conn.execute_batch("BEGIN").expect("begin txn 1");
        conn.execute_batch(merge_sql).expect("MERGE run 1");
        conn.execute_batch("COMMIT").expect("commit txn 1");
        let after_first = select_id_val(&conn, "t");
        assert_eq!(
            after_first,
            vec![(1, "a".to_string()), (2, "b".to_string())]
        );

        conn.execute_batch("BEGIN").expect("begin txn 2");
        conn.execute_batch(merge_sql)
            .expect("MERGE run 2 (identical re-delivery)");
        conn.execute_batch("COMMIT").expect("commit txn 2");
        let after_second = select_id_val(&conn, "t");

        assert_eq!(
            after_second, after_first,
            "identical re-delivery through a second, independently committed MERGE must not \
             change the table"
        );
    }

    #[test]
    fn merge_through_ducklake_rollback_restores_pre_merge_state_and_snapshots() {
        // THE GATE (ADR 0008 §5b / Open verification gate #1). Mirrors
        // `ducklake_rollback_restores_committed_state_and_snapshots` above,
        // substituting a MERGE for that test's CREATE OR REPLACE/INSERT
        // pair. If this fails, MERGE is not safe to wire into the run path
        // and --verify-replay's ROLLBACK-based diff (`verify_replay`,
        // ~2078-2175) cannot be trusted with it either — both rely on
        // exactly this ROLLBACK semantics.
        let (conn, _dir) = probe_attach("merge-rollback");

        conn.execute_batch(
            "CREATE TABLE t (id INTEGER, val VARCHAR); \
             INSERT INTO t SELECT id, 'baseline' FROM range(100) r(id);",
        )
        .expect("create baseline table t (100 rows)");
        let baseline_rows = select_id_val(&conn, "t");
        assert_eq!(baseline_rows.len(), 100);
        let baseline_snapshot = max_snapshot_id(&conn);

        conn.execute_batch("BEGIN;").expect("begin");
        conn.execute_batch(
            "MERGE INTO t USING (VALUES (0, 'clobbered'), (999, 'new')) AS s(id, val) \
             ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET val = s.val \
             WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val);",
        )
        .expect("MERGE inside txn (1 update + 1 insert)");

        // Read-your-own-writes: the uncommitted MERGE must be visible inside
        // the same transaction (mirrors the CREATE OR REPLACE probe above).
        let in_txn_count: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .expect("select t inside same txn");
        assert_eq!(
            in_txn_count, 101,
            "expected to read the uncommitted MERGE's inserted row within the same transaction"
        );

        conn.execute_batch("ROLLBACK;").expect("rollback");

        assert_eq!(
            select_id_val(&conn, "t"),
            baseline_rows,
            "t must be restored to its exact pre-MERGE content after ROLLBACK"
        );

        let after_snapshot = max_snapshot_id(&conn);
        assert_eq!(
            after_snapshot, baseline_snapshot,
            "a rolled-back MERGE must not leave behind a new snapshot"
        );
    }

    #[test]
    fn verify_replay_passes_on_a_replay_safe_upsert_transform() {
        // Exercised through the actual `verify_replay()` code path, not a
        // standalone probe: commits a `materialize: upsert` transform once,
        // then lets `verify_replay` re-run it inside its own
        // BEGIN...ROLLBACK and diff the before/after content. A correct,
        // key-unique `upsert` must be a no-op on identical re-delivery.
        let cell = materialize_test_cell(
            "replay-upsert",
            "  - sql: sql/fct.sql\n    materialize: upsert\n    key: [id]\n",
            &[(
                "fct.sql",
                "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, val)",
            )],
        );
        run_transforms(&cell, false).unwrap();
        verify_replay(&cell, false).expect("a key-unique upsert must be replay-safe");
    }

    #[test]
    fn verify_replay_still_fails_a_nondeterministic_transform_control() {
        // Control: verify_replay must still catch *something* real. The old
        // control here was a hand-written duplicating plain INSERT — no
        // longer representable (see the note above `materialize_test_cell`'s
        // verify_replay tests). The one way left to trip verify_replay
        // through the declarative surface is the documented determinism
        // caveat (docs/guides/incremental.md §6): a SELECT that isn't a
        // pure function of its inputs produces different content on the
        // real run vs. the replay, and `random()` guarantees that.
        let cell = materialize_test_cell(
            "replay-nondeterministic",
            "  - sql/fct.sql\n",
            &[("fct.sql", "SELECT id, random() AS r FROM range(5) r(id)")],
        );
        run_transforms(&cell, false).unwrap();
        let err = verify_replay(&cell, false).unwrap_err().to_string();
        assert!(err.contains("replay-unsafe transform"), "got: {err}");
        assert!(err.contains("'fct'"), "got: {err}");
    }

    #[test]
    fn merge_surprise_duplicate_key_delta_against_an_existing_row_silently_updates_once() {
        // ADR 0008 §5c originally claimed, as proven, that a duplicate-key
        // delta "makes MERGE error with 'cannot affect row a second time.'"
        // That claim did not hold on DuckDB 1.5.4 (this repo's bundled
        // version — libduckdb-sys 1.10504.0): `MERGE INTO ... WHEN MATCHED
        // THEN UPDATE` with two delta rows for one already-stored key
        // returned `Ok(())`, not an error — the gate test that encoded the
        // literal claim was removed once the ADR was corrected to have the
        // engine own the duplicate-key guarantee itself (a staged-delta
        // `GROUP BY key HAVING count(*) > 1` check, primitive-independent).
        // This test survives as the record of WHY that engine-owned check is
        // necessary: the target row for the duplicated key is silently
        // updated exactly once, from an unspecified one of the two delta
        // rows — not both applied sequentially. Confirmed via a
        // self-referencing `SET val = t.val || s.val` probe outside this
        // suite: the result was a single concatenation ("xa"), not a double
        // one ("xab"), which rules out "both rows applied in some order" and
        // pins "exactly one of the two rows is silently dropped." No error,
        // no warning, and the winner is not guaranteed to be delta-order
        // deterministic — so this assertion only pins "no error and exactly
        // one row survives for the key," not which value wins.
        let (conn, _dir) = probe_attach("merge-dupkey-matched-surprise");
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER, val VARCHAR); INSERT INTO t VALUES (1, 'orig');",
        )
        .expect("create baseline table t with a pre-existing row for key 1");

        conn.execute_batch("BEGIN;").expect("begin");
        conn.execute_batch(
            "MERGE INTO t USING (VALUES (1, 'a'), (1, 'b')) AS s(id, val) \
             ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET val = s.val \
             WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val);",
        )
        .expect(
            "MERGE against an existing row with a duplicate-key delta is expected \
             (surprisingly) to succeed, not error",
        );
        conn.execute_batch("COMMIT;").expect("commit");

        let rows = select_id_val(&conn, "t");
        assert_eq!(
            rows.len(),
            1,
            "expected exactly one surviving row for key 1, got {rows:?}"
        );
        assert_eq!(rows[0].0, 1);
        assert!(
            rows[0].1 == "a" || rows[0].1 == "b",
            "expected the surviving value to be one of the two delta rows, got {rows:?}"
        );
    }

    #[test]
    fn merge_surprise_a_duplicate_key_delta_against_no_existing_row_inserts_both_silently() {
        // A second facet of the same finding: ADR 0008 §5c states, without
        // qualification, that a duplicate-key delta makes MERGE error with
        // "cannot affect row a second time." The two tests above show that
        // claim does not hold even when both delta rows hit an EXISTING
        // target row via WHEN MATCHED. It fares no better when the key has
        // NO existing target row: both delta rows fall to WHEN NOT MATCHED
        // and DuckDB inserts BOTH of them — silently violating "one row per
        // key" and leaving two rows with the same key in the table. No
        // error, no warning.
        //
        // This matters for real `upsert` behavior: on a cold/empty
        // accumulator table (first-ever bootstrap run, or any key that has
        // never been delivered before), a non-key-unique SELECT does not
        // fail loudly the way §5c promises in ANY shape tested here.
        let (conn, _dir) = probe_attach("merge-dupkey-unmatched");
        conn.execute_batch("CREATE TABLE t (id INTEGER, val VARCHAR);")
            .expect("create baseline table t (empty — no row for key 1 yet)");

        conn.execute_batch("BEGIN;").expect("begin");
        conn.execute_batch(
            "MERGE INTO t USING (VALUES (1, 'a'), (1, 'b')) AS s(id, val) \
             ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET val = s.val \
             WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val);",
        )
        .expect(
            "MERGE against an empty target with a duplicate-key delta is expected (surprisingly) \
             to succeed, not error — that is exactly the finding this test pins",
        );
        conn.execute_batch("COMMIT;").expect("commit");

        assert_eq!(
            select_id_val(&conn, "t"),
            vec![(1, "a".to_string()), (1, "b".to_string())],
            "both duplicate-key delta rows were inserted, violating one-row-per-key silently"
        );
    }

    // --- ADR 0008: declarative incremental materialization -----------------
    //
    // Work item 3's regression suite: bootstrap, both strategies, replay
    // idempotence, every guard, `--full-refresh` reconciliation, raw+
    // declarative interleaving, and the gate-3 LIMIT-0 bootstrap canary.

    /// A minimal cell (no sources) whose `transforms:` block is exactly
    /// `transforms_yaml`, verbatim — unlike `replay_test_cell` (which always
    /// writes bare-string entries and commits once up front), this returns
    /// an **opened, uncommitted** `Cell` so a test can drive `run_transforms`
    /// itself, across as many separate transactions as it needs.
    fn materialize_test_cell(tag: &str, transforms_yaml: &str, sql_files: &[(&str, &str)]) -> Cell {
        let dir = probe_scratch_dir(tag);
        std::fs::create_dir_all(dir.join("sql")).unwrap();
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::write(
            dir.join("cell.yaml"),
            format!("cell: t\ntransforms:\n{transforms_yaml}\ninterface: []\n"),
        )
        .unwrap();
        std::fs::write(
            dir.join("profiles/local.yaml"),
            "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
        )
        .unwrap();
        for (f, sql) in sql_files {
            std::fs::write(dir.join("sql").join(f), sql).unwrap();
        }
        open(&dir.join("cell.yaml"), "local", false).expect("open test cell")
    }

    /// Overwrite a transform's SQL file content in place — how these tests
    /// simulate "the next run's delta" between two calls to
    /// `run_transforms`. Deliberately inline `VALUES` literals, never a
    /// file read: a declarative SQL file's relative paths resolve against
    /// the *process* cwd, not the cell directory (the engine wraps the
    /// text as an opaque subquery, ADR 0008 §2 — it never rewrites paths
    /// inside it the way `sources:` bindings do), so a `read_csv('x.csv')`
    /// in a test fixture would silently look in the wrong place.
    fn write_sql(cell: &Cell, file: &str, sql: &str) {
        std::fs::write(cell.dir.join("sql").join(file), sql).unwrap();
    }

    /// Every transform entry inside one `BEGIN...COMMIT` — mirrors `run`'s
    /// transaction wrapping (ADR 0005 §1) without the source-binding/
    /// verify/publish machinery a full `run()` call needs. Rolls back and
    /// propagates the error on failure, exactly like `run` would abort the
    /// whole snapshot rather than commit a partial one.
    fn run_transforms(cell: &Cell, full_refresh: bool) -> Result<()> {
        cell.conn.execute_batch("BEGIN")?;
        for t in &cell.transforms {
            if let Err(e) = execute_transform(&cell.conn, &cell.dir, t, full_refresh) {
                cell.conn.execute_batch("ROLLBACK")?;
                return Err(e);
            }
        }
        cell.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    #[test]
    fn materialize_bootstrap_creates_an_empty_correctly_typed_table_even_when_a_guard_then_fails() {
        // Isolates bootstrap from strategy population: a NULL key fails the
        // guard *after* bootstrap has already run (ADR 0008 §4), so the
        // table exists — empty, with the SELECT's exact shape — even though
        // nothing was ever written into it.
        let cell = materialize_test_cell(
            "mat-bootstrap-empty",
            "  - sql: sql/fct.sql\n    materialize: upsert\n    key: [id]\n",
            &[(
                "fct.sql",
                "SELECT * FROM (VALUES (1, 'a'), (NULL, 'b')) AS t(id, val)",
            )],
        );
        // Inspected read-your-own-writes, inside the transaction, before
        // rolling back: `run_transforms` (like `run`) rolls back the whole
        // transaction on any transform error, which would undo the
        // bootstrap DDL too and defeat the point of this test.
        cell.conn.execute_batch("BEGIN").unwrap();
        let err = execute_transform(&cell.conn, &cell.dir, &cell.transforms[0], false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("contains NULL"), "got: {err}");

        let cols = describe_shape(&cell.conn, "\"fct\"").expect("describe bootstrapped table");
        assert_eq!(
            cols,
            vec![
                ("id".to_string(), "INTEGER".to_string()),
                ("val".to_string(), "VARCHAR".to_string()),
            ]
        );
        let count: i64 = cell
            .conn
            .query_row("SELECT count(*) FROM \"fct\"", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "bootstrap must create an empty table");
        cell.conn.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn materialize_upsert_accumulates_across_two_runs() {
        let cell = materialize_test_cell(
            "mat-upsert-accum",
            "  - sql: sql/fct.sql\n    materialize: upsert\n    key: [id]\n",
            &[(
                "fct.sql",
                "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, val)",
            )],
        );
        run_transforms(&cell, false).unwrap();
        assert_eq!(
            select_id_val(&cell.conn, "fct"),
            vec![(1, "a".to_string()), (2, "b".to_string())]
        );

        // Run 2: a different delta — key 2 updates, key 3 is new, key 1
        // (absent from this delta) is untouched.
        write_sql(
            &cell,
            "fct.sql",
            "SELECT * FROM (VALUES (2, 'b2'), (3, 'c')) AS t(id, val)",
        );
        run_transforms(&cell, false).unwrap();
        assert_eq!(
            select_id_val(&cell.conn, "fct"),
            vec![
                (1, "a".to_string()),
                (2, "b2".to_string()),
                (3, "c".to_string()),
            ]
        );
    }

    #[test]
    fn materialize_upsert_is_idempotent_across_two_runs_with_the_identical_delta() {
        let cell = materialize_test_cell(
            "mat-upsert-idempotent",
            "  - sql: sql/fct.sql\n    materialize: upsert\n    key: [id]\n",
            &[(
                "fct.sql",
                "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, val)",
            )],
        );
        run_transforms(&cell, false).unwrap();
        let after_first = select_id_val(&cell.conn, "fct");

        // Identical re-delivery (lookback re-read, rollback-then-rerun) —
        // the second, independently committed run must leave the table
        // unchanged (ADR 0008 §3: replay-safe by construction).
        run_transforms(&cell, false).unwrap();
        assert_eq!(select_id_val(&cell.conn, "fct"), after_first);
    }

    #[test]
    fn materialize_append_ignores_redelivered_keys() {
        let cell = materialize_test_cell(
            "mat-append",
            "  - sql: sql/fct.sql\n    materialize: append\n    key: [id]\n",
            &[(
                "fct.sql",
                "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, val)",
            )],
        );
        run_transforms(&cell, false).unwrap();

        // Re-delivery of key 1 with a DIFFERENT value, plus a genuinely new
        // key 3 — `append` is insert-only: existing rows are never touched,
        // so key 1 must keep its original value, not the re-delivered one.
        write_sql(
            &cell,
            "fct.sql",
            "SELECT * FROM (VALUES (1, 'a-changed'), (3, 'c')) AS t(id, val)",
        );
        run_transforms(&cell, false).unwrap();
        assert_eq!(
            select_id_val(&cell.conn, "fct"),
            vec![
                (1, "a".to_string()),
                (2, "b".to_string()),
                (3, "c".to_string()),
            ],
            "append must drop the re-delivered key 1 row unchanged, never apply its new value"
        );
    }

    #[test]
    fn materialize_null_key_errors_with_the_adr_exact_message() {
        let cell = materialize_test_cell(
            "mat-null-key",
            "  - sql: sql/fct.sql\n    materialize: upsert\n    key: [flight_id]\n",
            &[(
                "fct.sql",
                "SELECT * FROM (VALUES (1, 'a'), (NULL, 'b'), (NULL, 'c')) AS t(flight_id, val)",
            )],
        );
        let err = run_transforms(&cell, false).unwrap_err().to_string();
        assert!(err.contains("sql/fct.sql"), "got: {err}");
        assert!(
            err.contains(
                "materialize key column 'flight_id' contains NULL in the staged delta (2 rows)"
            ),
            "got: {err}"
        );
        assert!(
            err.contains("Make 'flight_id' NOT NULL upstream"),
            "got: {err}"
        );
    }

    #[test]
    fn materialize_duplicate_key_errors_naming_offending_values() {
        let cell = materialize_test_cell(
            "mat-dup-key",
            "  - sql: sql/fct.sql\n    materialize: upsert\n    key: [id]\n",
            &[(
                "fct.sql",
                "SELECT * FROM (VALUES (1, 'a'), (1, 'b'), (2, 'c')) AS t(id, val)",
            )],
        );
        cell.conn.execute_batch("BEGIN").unwrap();
        let err = execute_transform(&cell.conn, &cell.dir, &cell.transforms[0], false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("sql/fct.sql"), "got: {err}");
        assert!(
            err.contains("is not unique in the staged delta"),
            "got: {err}"
        );
        assert!(err.contains('1'), "got: {err}");
        assert!(err.contains("QUALIFY row_number() OVER"), "got: {err}");

        // The table must exist (bootstrap ran) but stay empty — the guard
        // fired before any strategy DML touched it. Inspected before
        // rollback, same reason as the bootstrap-isolation test above.
        let count: i64 = cell
            .conn
            .query_row("SELECT count(*) FROM \"fct\"", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        cell.conn.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn materialize_schema_drift_errors_naming_the_new_column_and_recovery_routes() {
        let cell = materialize_test_cell(
            "mat-drift",
            "  - sql: sql/fct.sql\n    materialize: upsert\n    key: [id]\n",
            &[("fct.sql", "SELECT * FROM (VALUES (1, 'a')) AS t(id, val)")],
        );
        run_transforms(&cell, false).unwrap();

        // The SELECT now yields an extra column the accumulated table
        // doesn't have.
        write_sql(
            &cell,
            "fct.sql",
            "SELECT * FROM (VALUES (2, 'b', 'AA')) AS t(id, val, carrier)",
        );
        let err = run_transforms(&cell, false).unwrap_err().to_string();
        assert!(err.contains("declarative table 'fct'"), "got: {err}");
        assert!(
            err.contains("the SELECT now yields column 'carrier'"),
            "got: {err}"
        );
        assert!(
            err.contains("does not migrate schema in place"),
            "got: {err}"
        );
        assert!(
            err.contains("no ALTER path inside the pipeline"),
            "got: {err}"
        );
        assert!(err.contains("--full-refresh"), "got: {err}");
        assert!(err.contains("datamk attach"), "got: {err}");

        // The pre-drift row must be untouched — the error fired before any
        // strategy DML.
        assert_eq!(select_id_val(&cell.conn, "fct"), vec![(1, "a".to_string())]);
    }

    #[test]
    fn materialize_a_trailing_semicolon_in_the_sql_file_fails_loudly_not_silently() {
        // ADR 0008 §2: the engine wraps the file's text as a subquery
        // (`... AS (<file text>)`), never parsing it, so a file containing
        // anything but one bare SELECT — including a trailing `;`, the most
        // reflexive SQL-authoring habit there is — produces a composed
        // statement that fails to prepare, loudly, at run time. Found while
        // building the `init` scaffold's own declarative demo file, which
        // hit exactly this. Pinned here as a regression, not left as tribal
        // knowledge.
        let cell = materialize_test_cell(
            "mat-trailing-semicolon",
            "  - sql: sql/fct.sql\n    materialize: upsert\n    key: [id]\n",
            &[(
                "fct.sql",
                "SELECT * FROM (VALUES (1, 'a')) AS t(id, val);\n",
            )],
        );
        let err = run_transforms(&cell, false).unwrap_err();
        let chain = format!("{err:?}"); // anyhow's Debug prints the full "Caused by" chain
        assert!(chain.contains("sql/fct.sql"), "got: {chain}");
        assert!(
            chain.contains("syntax error"),
            "expected a loud parse error, got: {chain}"
        );
        assert!(
            chain.contains("transform files contain exactly one SELECT"),
            "got: {chain}"
        );
        assert!(
            chain.contains("Remove the trailing semicolon"),
            "got: {chain}"
        );
        assert!(
            chain.contains("docs/guides/incremental.md §4"),
            "got: {chain}"
        );
    }

    #[test]
    fn materialize_a_different_parse_error_does_not_get_the_misleading_semicolon_hint() {
        // The hint (above) is a pure string check on the file text — "does
        // it end with `;`" — never SQL parsing, so it must not fire for an
        // unrelated parse error (a typo'd keyword) whose file has no
        // trailing semicolon at all. A wrong hint here is worse than no
        // hint.
        let cell = materialize_test_cell(
            "mat-parse-error-no-semicolon",
            "  - sql: sql/fct.sql\n    materialize: upsert\n    key: [id]\n",
            &[("fct.sql", "SELCT * FROM (VALUES (1, 'a')) AS t(id, val)")],
        );
        let err = run_transforms(&cell, false).unwrap_err();
        let chain = format!("{err:?}");
        assert!(chain.contains("sql/fct.sql"), "got: {chain}");
        assert!(
            !chain.contains("remove the trailing semicolon"),
            "must not suggest the semicolon fix for an unrelated parse error: {chain}"
        );
    }

    #[test]
    fn materialize_full_refresh_rebuild_reconciles_a_hard_delete() {
        // ADR 0008 §6: anti-join/upsert alone never remove upstream-deleted
        // rows — they simply stop being delivered. `--full-refresh`'s
        // from-scratch rebuild is the reconciliation lever.
        let cell = materialize_test_cell(
            "mat-full-refresh",
            "  - sql: sql/fct.sql\n    materialize: upsert\n    key: [id]\n",
            &[(
                "fct.sql",
                "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, val)",
            )],
        );
        run_transforms(&cell, false).unwrap();
        assert_eq!(
            select_id_val(&cell.conn, "fct"),
            vec![(1, "a".to_string()), (2, "b".to_string())]
        );

        // Upstream hard-deleted id=1; a normal (non-full-refresh) run over
        // the surviving row alone must leave the ghost behind.
        write_sql(
            &cell,
            "fct.sql",
            "SELECT * FROM (VALUES (2, 'b')) AS t(id, val)",
        );
        run_transforms(&cell, false).unwrap();
        assert_eq!(
            select_id_val(&cell.conn, "fct"),
            vec![(1, "a".to_string()), (2, "b".to_string())],
            "a normal run must not reconcile a hard delete — id=1 is a ghost until --full-refresh"
        );

        // `--full-refresh` rebuilds from the full (unfiltered) re-read and
        // drops the ghost.
        run_transforms(&cell, true).unwrap();
        assert_eq!(
            select_id_val(&cell.conn, "fct"),
            vec![(2, "b".to_string())],
            "--full-refresh must rebuild from scratch and drop the hard-deleted row"
        );
    }

    // --- ADR 0008: the uniform naming invariant (raw-path tests removed) ---
    //
    // `check_uniform_naming` (the raw-entry post-run catalog-diff check) and
    // every test that exercised it — a stem/build mismatch, a multi-table
    // file, an ALTER-only migration — are gone along with the raw execution
    // branch itself. Table = file stem is enforced by construction for
    // every transform now (`resolve_transforms`'s `claim_table`, schema.rs);
    // there is no longer a way to *build the wrong table*, so there is
    // nothing left to observe after the fact. See ADR 0008 decisions 1–2.

    #[test]
    fn bare_path_and_mapping_entries_interleave_in_listed_order() {
        // Both `TransformEntry` syntaxes are one language now — a bare path
        // (`materialize: replace`, implied) freely interleaves with an
        // explicit `materialize:` mapping in one list, executed in exactly
        // that order.
        let cell = materialize_test_cell(
            "mat-interleave",
            "  - sql/stg.sql\n  - sql: sql/fct.sql\n    materialize: upsert\n    key: [id]\n  - sql/post.sql\n",
            &[
                ("stg.sql", "SELECT * FROM (VALUES (1, 'a')) AS t(id, val)"),
                ("fct.sql", "SELECT * FROM stg"),
                ("post.sql", "SELECT count(*) AS n FROM fct"),
            ],
        );
        run_transforms(&cell, false).unwrap();

        assert_eq!(select_id_val(&cell.conn, "fct"), vec![(1, "a".to_string())]);
        let n: i64 = cell
            .conn
            .query_row("SELECT n FROM post", [], |r| r.get(0))
            .expect("the bare-path entry after the mapping entry must see fct's committed row");
        assert_eq!(
            n, 1,
            "post.sql runs after fct materializes — listed order must hold across syntaxes"
        );
    }

    /// ADR 0008, end to end, the founder's target shape: one cell, an
    /// `incremental:` source, an `upsert` accumulator reading it, and a
    /// `replace` rollup reading the accumulator (never the source) — gate
    /// 4c silent, `datamk verify` green, and every `sql/` file zero-CREATE,
    /// exactly like a real cell's `sql/` directory. Goes through the real
    /// `config::load` seam (`engine::open`), so `resolve_transforms`, guard
    /// 4c, and grain inheritance all run for real, not simulated. The one
    /// thing this test cannot do without live warehouse credentials is
    /// `bind_source` itself — so it manufactures the `events` TEMP VIEW by
    /// hand, exactly the shape `bind_source`/`stage_incremental` would leave
    /// behind for a transform to read (see `stage_incremental_*` tests,
    /// same technique), and runs the pipeline across two simulated
    /// deliveries (bootstrap, then a delta with an update and a new row).
    #[test]
    fn e2e_incremental_source_upsert_accumulator_and_replace_rollup_over_it() {
        let dir = probe_scratch_dir("e2e-incremental-materialize");
        std::fs::create_dir_all(dir.join("sql")).unwrap();
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::write(
            dir.join("cell.yaml"),
            "cell: t\n\
             sources:\n\
             \x20 events:\n\
             \x20   connection: crm\n\
             \x20   table: analytics.events\n\
             \x20   incremental:\n\
             \x20     cursor: updated_at\n\
             transforms:\n\
             \x20 - sql: sql/fct_events.sql\n\
             \x20   materialize: upsert\n\
             \x20   key: [event_id]\n\
             \x20 - sql/events_daily.sql\n\
             interface:\n\
             \x20 - name: fct_events\n\
             \x20   version: 1.0.0\n\
             \x20 - name: events_daily\n\
             \x20   version: 1.0.0\n\
             \x20   grain: [day]\n",
        )
        .unwrap();
        let fct_events_sql = "SELECT event_id, region, revenue FROM events";
        let events_daily_sql = "SELECT region AS day, count(*) AS n, sum(revenue) AS total \
                                 FROM fct_events GROUP BY 1";
        std::fs::write(dir.join("sql/fct_events.sql"), fct_events_sql).unwrap();
        std::fs::write(dir.join("sql/events_daily.sql"), events_daily_sql).unwrap();
        // Zero CREATE statements anywhere in the author-owned SQL — the same
        // property the `init` scaffold's own test pins (ADR 0008 decision 1).
        for sql in [fct_events_sql, events_daily_sql] {
            assert!(
                !sql.to_uppercase().contains("CREATE"),
                "author SQL must be SELECT-only: {sql}"
            );
        }
        std::fs::write(
            dir.join("profiles/local.yaml"),
            "catalog: ./.cell/catalog.ducklake\n\
             storage: ./.cell/data\n\
             connections:\n\
             \x20 crm:\n\
             \x20   type: bigquery\n\
             \x20   project: test-project\n",
        )
        .unwrap();

        // `engine::open` -> `config::load`: resolve_transforms, guard 4c,
        // and grain inheritance all run here, for real, before anything
        // below touches a row. If guard 4c mis-fired on `fct_events`
        // (contains "events" as a substring, not a token) or on the upsert
        // model itself (delta consumers are exempt), `open` would return
        // `Err` and this test would fail right here — "gate silent" is
        // proven by `open` succeeding, not asserted separately.
        let cell = open(&dir.join("cell.yaml"), "local", false).expect(
            "gate 4c must stay silent: the upsert model is exempt, and the replace \
                     rollup reads the accumulator table, never the source, by name",
        );

        // Grain inheritance: fct_events (upsert) inherits `key:`; events_daily
        // (replace) keeps its explicit `grain:` — no key to inherit from.
        assert_eq!(
            cell.def.interface[0].grain,
            vec!["event_id".to_string()],
            "the upsert accumulator's export must inherit grain from `key:`"
        );
        assert_eq!(
            cell.def.interface[1].grain,
            vec!["day".to_string()],
            "the replace rollup's export must keep its explicit grain unchanged"
        );

        // Manufacture the `events` source view by hand — what a real
        // `bind_source` call against a live warehouse would have left
        // behind for the transform to read. Bootstrap delta: two rows.
        cell.conn
            .execute_batch(
                "CREATE OR REPLACE TEMP VIEW \"events\" AS SELECT * FROM (VALUES \
                 (1, 'us-east', 10.0), (2, 'us-west', 20.0)) AS t(event_id, region, revenue);",
            )
            .unwrap();
        run_transforms(&cell, false).expect("bootstrap run must build both tables cleanly");

        let fct: Vec<(i64, String, f64)> = cell
            .conn
            .prepare("SELECT event_id, region, revenue FROM fct_events ORDER BY event_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<duckdb::Result<_>>()
            .unwrap();
        assert_eq!(
            fct,
            vec![
                (1, "us-east".to_string(), 10.0),
                (2, "us-west".to_string(), 20.0),
            ]
        );

        // Green: `datamk verify`'s actual check, not just "it ran".
        crate::verify::check(&cell.conn, &cell.def)
            .expect("bootstrap output must verify cleanly against the declared interface");

        // Second delivery: an update to key 1 (upsert must replace, not
        // duplicate) plus a brand-new key 3. Simulates what the next run's
        // staged delta would contain — a re-delivery under lookback, or a
        // genuinely new incremental read past the watermark.
        cell.conn
            .execute_batch(
                "CREATE OR REPLACE TEMP VIEW \"events\" AS SELECT * FROM (VALUES \
                 (1, 'us-east', 15.0), (3, 'eu-west', 30.0)) AS t(event_id, region, revenue);",
            )
            .unwrap();
        run_transforms(&cell, false).expect("second run (re-delivery + new row) must be clean");

        let fct2: Vec<(i64, String, f64)> = cell
            .conn
            .prepare("SELECT event_id, region, revenue FROM fct_events ORDER BY event_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<duckdb::Result<_>>()
            .unwrap();
        assert_eq!(
            fct2,
            vec![
                (1, "us-east".to_string(), 15.0), // updated in place, not duplicated
                (2, "us-west".to_string(), 20.0), // untouched
                (3, "eu-west".to_string(), 30.0), // newly accumulated
            ],
            "upsert must reconcile the re-delivered key and accumulate the new one, in the \
             same accumulator across two runs"
        );

        // events_daily is a `replace` rollup over fct_events (the
        // accumulator, three rows now) — every run recomputes it fresh from
        // current state, never from the delta.
        let n_groups: i64 = cell
            .conn
            .query_row("SELECT count(*) FROM events_daily", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            n_groups, 3,
            "one group per distinct region across all 3 accumulated rows"
        );

        crate::verify::check(&cell.conn, &cell.def)
            .expect("post-accumulation output must still verify cleanly");
    }

    #[test]
    fn bootstrap_limit_0_short_circuits_an_expensive_select_the_canary_gate_3_requires() {
        // ADR 0008 Open verification gates §3 (CLEARED, empirical, 2026-07-13):
        // `CREATE TABLE IF NOT EXISTS ... AS <select> LIMIT 0` short-circuits
        // rather than fully evaluating an expensive SELECT — proven at 80M
        // rows (9,579x for a window aggregate). This canary regresses that
        // claim at a CI-friendly scale: a 10M-row windowed aggregate staged
        // into a real TEMP table (staging always fully evaluates — that's
        // the delta, not what this canary is about), then bootstrap reading
        // `LIMIT 0` from it. The ADR's own claim is sub-10ms; this asserts a
        // much more generous bound to stay stable on a loaded CI box while
        // still catching a real regression — an in-session full
        // materialization of the same relation measured ~9x slower locally.
        let (conn, _dir) = probe_attach("bootstrap-canary");
        conn.execute_batch(
            "CREATE TEMP TABLE stg AS SELECT id, \
               sum(id) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running, \
               avg(id) OVER (ORDER BY id ROWS BETWEEN 100 PRECEDING AND CURRENT ROW) AS avgw \
             FROM range(10000000) r(id);",
        )
        .expect("stage a 10M-row windowed relation");

        let started = Instant::now();
        conn.execute_batch("CREATE TABLE IF NOT EXISTS bootstrapped AS SELECT * FROM stg LIMIT 0;")
            .expect("bootstrap DDL");
        let elapsed = started.elapsed();

        let count: i64 = conn
            .query_row("SELECT count(*) FROM bootstrapped", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "bootstrap must create an empty table");
        assert!(
            elapsed < Duration::from_millis(500),
            "bootstrap took {elapsed:?} against a 10M-row staged relation — gate 3 expects this \
             to short-circuit LIMIT 0, not evaluate; a planner regression may have reintroduced \
             full evaluation on every declarative bootstrap"
        );
    }

    // --- ADR 0008 §7 (amended): the written eject artifact ------------------
    //
    // Gate 2 (eject bit-identity proof) failed its first pass on the
    // *recipe*, not the data identity: a bare `CREATE TEMP TABLE` staging
    // statement errored "already exists" when the pasted eject artifact was
    // executed twice in one connection — exactly what `--verify-replay`
    // does to every entry's staged statement. Fixed with `CREATE OR REPLACE
    // TEMP TABLE`. Verbatim multi-line SQL in a log line also proved
    // un-greppable in practice, so the artifact moved from a logged line to
    // a written file, `.cell/materialize/<table>.sql`. These tests read the
    // file the engine actually wrote — via `materialize_test_cell` +
    // `run_transforms`, the real production path — rather than
    // hand-assembling "pasted" text, so they prove the actual artifact: an
    // audit/portability surface (ADR §7), not a migration path — whatever's
    // on disk is exactly what ran, nothing more.

    #[test]
    fn materialize_writes_the_eject_artifact_with_the_required_header() {
        let cell = materialize_test_cell(
            "eject-artifact-header",
            "  - sql: sql/fct.sql\n    materialize: upsert\n    key: [id]\n",
            &[(
                "fct.sql",
                "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, val)",
            )],
        );
        run_transforms(&cell, false).unwrap();

        let artifact_path = cell.dir.join(".cell").join("materialize").join("fct.sql");
        let artifact = std::fs::read_to_string(&artifact_path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", artifact_path.display()));

        // (a) this file is the exact DML executed this run for the table.
        assert!(artifact.contains("fct"), "got: {artifact}");
        assert!(
            artifact.contains("exact DML the engine executed THIS RUN"),
            "got: {artifact}"
        );
        // (b) the engine-side guards are not in this file.
        assert!(
            artifact.contains("NULL-key, duplicate-key, and schema-drift"),
            "got: {artifact}"
        );
        assert!(artifact.contains("are NOT in this file"), "got: {artifact}");
        // (c) audit/portability framing, not a migration path (ADR 0008 §7)
        // — no supported way to point cell.yaml at this file directly.
        assert!(
            artifact.contains("audit/portability artifact, not a migration path"),
            "got: {artifact}"
        );
        assert!(artifact.contains("no supported way to"), "got: {artifact}");
        assert!(
            artifact.contains("point cell.yaml `transforms:` at this file directly"),
            "got: {artifact}"
        );
        // (d) pointer to the guide.
        assert!(
            artifact.contains("docs/guides/incremental.md §4"),
            "got: {artifact}"
        );
        // The three statements, in order, runnable as pasted.
        let staging_pos = artifact.find("CREATE OR REPLACE TEMP TABLE").unwrap();
        let bootstrap_pos = artifact.find("CREATE TABLE IF NOT EXISTS").unwrap();
        let merge_pos = artifact.find("MERGE INTO").unwrap();
        assert!(
            staging_pos < bootstrap_pos && bootstrap_pos < merge_pos,
            "expected staging, then bootstrap, then strategy, got: {artifact}"
        );
    }

    #[test]
    fn eject_artifact_upsert_is_idempotent_under_same_connection_re_execution() {
        let cell = materialize_test_cell(
            "eject-artifact-upsert",
            "  - sql: sql/fct.sql\n    materialize: upsert\n    key: [id]\n",
            &[(
                "fct.sql",
                "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, val)",
            )],
        );
        run_transforms(&cell, false).unwrap();

        let artifact_path = cell.dir.join(".cell").join("materialize").join("fct.sql");
        let pasted = std::fs::read_to_string(&artifact_path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", artifact_path.display()));

        // Paste into an independent connection, seeded to mimic an existing
        // accumulator, and execute the artifact **exactly as written**
        // (header comments included, nothing stripped) twice — exactly
        // what --verify-replay does to every entry's staged statement.
        let (conn, _dir) = probe_attach("eject-idempotent-upsert-paste");
        conn.execute_batch(
            "CREATE TABLE fct (id INTEGER, val VARCHAR); INSERT INTO fct VALUES (1, 'orig');",
        )
        .unwrap();

        conn.execute_batch(&pasted)
            .expect("first execution of the pasted eject artifact");
        let after_first = select_id_val(&conn, "fct");
        assert_eq!(
            after_first,
            vec![(1, "a".to_string()), (2, "b".to_string())]
        );

        conn.execute_batch(&pasted).expect(
            "the pasted eject artifact must survive same-connection re-execution (exactly what \
             --verify-replay does to every entry's staged statement) — this is the regression \
             the first gate-2 eject proof attempt found: `CREATE TEMP TABLE` without \
             `OR REPLACE` errored \"already exists\" on the second run",
        );
        assert_eq!(
            select_id_val(&conn, "fct"),
            after_first,
            "re-executing the identical pasted artifact must leave the table unchanged"
        );
    }

    #[test]
    fn eject_artifact_append_is_idempotent_under_same_connection_re_execution() {
        let cell = materialize_test_cell(
            "eject-artifact-append",
            "  - sql: sql/fct.sql\n    materialize: append\n    key: [id]\n",
            &[(
                "fct.sql",
                "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, val)",
            )],
        );
        run_transforms(&cell, false).unwrap();

        let artifact_path = cell.dir.join(".cell").join("materialize").join("fct.sql");
        let pasted = std::fs::read_to_string(&artifact_path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", artifact_path.display()));
        assert!(pasted.contains("ANTI JOIN"), "got: {pasted}");

        let (conn, _dir) = probe_attach("eject-idempotent-append-paste");
        conn.execute_batch("CREATE TABLE fct (id INTEGER, val VARCHAR);")
            .unwrap();

        conn.execute_batch(&pasted)
            .expect("first execution of the pasted eject artifact");
        let after_first = select_id_val(&conn, "fct");
        assert_eq!(
            after_first,
            vec![(1, "a".to_string()), (2, "b".to_string())]
        );

        conn.execute_batch(&pasted)
            .expect("the pasted append eject artifact must survive same-connection re-execution");
        assert_eq!(
            select_id_val(&conn, "fct"),
            after_first,
            "append's anti-join must drop both re-delivered keys on the second execution"
        );
    }

    #[test]
    fn write_eject_artifact_returns_the_resolved_path_under_dot_cell() {
        // `.cell/materialize/<table>.sql` resolves against `dir` (the cell
        // directory), exactly like the catalog file and the release
        // manifest — the path `log_eject_notice` then points at (no
        // `tracing` capture harness exists in this codebase to assert the
        // one-line log text directly; verified by manual `RUST_LOG=info`
        // run instead — see the ADR 0008 report).
        let cell = materialize_test_cell(
            "eject-artifact-path",
            "  - sql: sql/fct.sql\n    materialize: upsert\n    key: [id]\n",
            &[("fct.sql", "SELECT * FROM (VALUES (1, 'a')) AS t(id, val)")],
        );
        let artifact = write_eject_artifact(
            &cell.dir,
            "fct",
            &["id".to_string()],
            &["-- irrelevant for this test".to_string()],
        )
        .unwrap();
        assert_eq!(
            artifact,
            cell.dir.join(".cell").join("materialize").join("fct.sql")
        );
        assert!(artifact.exists());
    }

    // --- ADR 0008 §3 (founder-ratified): `materialize: replace` ------------

    #[test]
    fn materialize_replace_rebuilds_correctly_across_two_runs_reflecting_upstream_deletion() {
        let cell = materialize_test_cell(
            "mat-replace-rebuild",
            "  - sql: sql/fct.sql\n    materialize: replace\n",
            &[(
                "fct.sql",
                "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, val)",
            )],
        );
        run_transforms(&cell, false).unwrap();
        assert_eq!(
            select_id_val(&cell.conn, "fct"),
            vec![(1, "a".to_string()), (2, "b".to_string())]
        );

        // Upstream "deleted" id=1 — replace's full rebuild must drop it.
        // Unlike upsert/append (which never remove upstream deletions —
        // ADR 0008 §6), this is replace's whole point: no reconciliation
        // step needed, the rebuild IS the reconciliation.
        write_sql(
            &cell,
            "fct.sql",
            "SELECT * FROM (VALUES (2, 'b')) AS t(id, val)",
        );
        run_transforms(&cell, false).unwrap();
        assert_eq!(
            select_id_val(&cell.conn, "fct"),
            vec![(2, "b".to_string())],
            "a normal (non-full-refresh) replace run must still fully rebuild — no ghost rows"
        );
    }

    #[test]
    fn materialize_replace_under_full_refresh_is_identical_to_a_normal_run() {
        // ADR 0008 §3/work item 3: replace gets no special --full-refresh
        // branch (it's already a full rebuild every run) — a run with the
        // flag set must produce byte-identical results to one without it.
        let normal = materialize_test_cell(
            "mat-replace-normal",
            "  - sql: sql/fct.sql\n    materialize: replace\n",
            &[(
                "fct.sql",
                "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, val)",
            )],
        );
        run_transforms(&normal, false).unwrap();

        let full_refresh = materialize_test_cell(
            "mat-replace-full-refresh",
            "  - sql: sql/fct.sql\n    materialize: replace\n",
            &[(
                "fct.sql",
                "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, val)",
            )],
        );
        run_transforms(&full_refresh, true).unwrap();

        let normal_result = select_id_val(&normal.conn, "fct");
        let full_refresh_result = select_id_val(&full_refresh.conn, "fct");
        assert_eq!(normal_result, full_refresh_result);
        assert_eq!(
            normal_result,
            vec![(1, "a".to_string()), (2, "b".to_string())]
        );
    }

    #[test]
    fn materialize_replace_writes_the_eject_artifact_with_the_simplified_header() {
        let cell = materialize_test_cell(
            "eject-artifact-replace-header",
            "  - sql: sql/fct.sql\n    materialize: replace\n",
            &[("fct.sql", "SELECT * FROM (VALUES (1, 'a')) AS t(id, val)")],
        );
        run_transforms(&cell, false).unwrap();

        let artifact_path = cell.dir.join(".cell").join("materialize").join("fct.sql");
        let artifact = std::fs::read_to_string(&artifact_path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", artifact_path.display()));

        assert!(
            artifact.contains("exact DML the engine executed THIS RUN"),
            "got: {artifact}"
        );
        // The simplified (b): no guards to lose, because none ever ran.
        assert!(
            artifact.contains("NULL-key/duplicate-key/schema-drift guards ever ran"),
            "got: {artifact}"
        );
        assert!(
            !artifact.contains("A pasted copy of the statements below does not"),
            "replace's header must not claim guards were lost — none ever ran: {artifact}"
        );
        assert!(
            artifact.contains("audit/portability artifact, not a migration path"),
            "got: {artifact}"
        );
        assert!(
            artifact.contains("docs/guides/incremental.md §4"),
            "got: {artifact}"
        );
        // One statement — no staging, no bootstrap.
        assert!(
            artifact.contains("CREATE OR REPLACE TABLE"),
            "got: {artifact}"
        );
        assert!(
            !artifact.contains("CREATE OR REPLACE TEMP TABLE"),
            "replace has no staging relation: {artifact}"
        );
        assert!(
            !artifact.contains("CREATE TABLE IF NOT EXISTS"),
            "replace has no bootstrap statement: {artifact}"
        );
    }

    #[test]
    fn eject_artifact_replace_is_idempotent_under_same_connection_re_execution() {
        let cell = materialize_test_cell(
            "eject-artifact-replace",
            "  - sql: sql/fct.sql\n    materialize: replace\n",
            &[(
                "fct.sql",
                "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, val)",
            )],
        );
        run_transforms(&cell, false).unwrap();

        let artifact_path = cell.dir.join(".cell").join("materialize").join("fct.sql");
        let pasted = std::fs::read_to_string(&artifact_path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", artifact_path.display()));

        let (conn, _dir) = probe_attach("eject-idempotent-replace-paste");
        conn.execute_batch(
            "CREATE TABLE fct (id INTEGER, val VARCHAR); INSERT INTO fct VALUES (1, 'orig');",
        )
        .unwrap();

        conn.execute_batch(&pasted)
            .expect("first execution of the pasted eject artifact");
        let after_first = select_id_val(&conn, "fct");
        assert_eq!(
            after_first,
            vec![(1, "a".to_string()), (2, "b".to_string())]
        );

        conn.execute_batch(&pasted)
            .expect("the pasted replace eject artifact must survive same-connection re-execution");
        assert_eq!(
            select_id_val(&conn, "fct"),
            after_first,
            "re-executing the identical pasted CREATE OR REPLACE must leave the table unchanged"
        );
    }
}
