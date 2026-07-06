mod connectors;

use anyhow::{Context, Result};
use duckdb::Connection;
use indexmap::IndexMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{CellDef, ResolvedBindings, ResolvedIncremental, ResolvedS3, ResolvedSource};
use crate::store::Store;

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
    /// Resolved token->roles file path, if configured.
    pub principals: Option<String>,
    /// The profile's `s3:` block; drives both DuckDB's secret and the native
    /// object-store client (ADR 0004 §3 credential parity).
    pub s3: Option<ResolvedS3>,
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

/// A unique, process-scoped local scratch directory.
fn new_scratch_dir(tag: &str) -> Result<PathBuf> {
    let safe: String = tag
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let dir = std::env::temp_dir().join(format!(
        "datamk-{safe}-{}-{}",
        std::process::id(),
        SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating scratch dir {}", dir.display()))?;
    Ok(dir)
}

pub fn open(file: &Path, profile: &str, read_only: bool) -> Result<Cell> {
    // The pure parse+resolve prefix lives in `config::load` (no DB); `open` is
    // that plus a connection. `deploy` uses `config::load` directly to inspect a
    // cell without ever opening a database.
    let loaded = crate::config::load(file, profile)?;
    let conn = Connection::open_in_memory().context("opening DuckDB")?;
    let scratch = new_scratch_dir(&loaded.def.cell)?;
    let published = setup(&conn, &loaded.bindings, &loaded.dir, read_only, &scratch)?;
    Ok(Cell {
        def: loaded.def,
        conn,
        dir: loaded.dir,
        sources: loaded.bindings.sources.clone(),
        principals: loaded.bindings.principals.clone(),
        s3: loaded.bindings.s3.clone(),
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

    // Object storage needs httpfs; S3 also needs a secret (default: AWS credential chain).
    let uses_remote = crate::config::is_remote(&storage)
        || b.sources.values().any(|s| s.is_remote())
        || b.s3.is_some();
    if uses_remote {
        conn.execute_batch("INSTALL httpfs; LOAD httpfs;")
            .context("loading httpfs extension")?;
        create_s3_secret(conn, b.s3.as_ref())?;
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
            let store = Arc::new(Store::for_storage(&storage, b.s3.as_ref())?);
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
pub fn open_artifact(local: &Path, storage: &str, s3: Option<&ResolvedS3>) -> Result<Connection> {
    let conn = Connection::open_in_memory().context("opening DuckDB")?;
    conn.execute_batch(
        "INSTALL ducklake; LOAD ducklake; INSTALL json; LOAD json; \
         INSTALL httpfs; LOAD httpfs; SET TimeZone = 'UTC';",
    )
    .context("loading extensions")?;
    create_s3_secret(&conn, s3)?;
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

/// Register an S3 secret in DuckDB's Secrets Manager. With explicit key/secret we
/// use static credentials; otherwise DuckDB's `credential_chain` provider resolves
/// AWS env vars, shared profiles, and IAM roles — no secrets in the cell config.
fn create_s3_secret(conn: &Connection, s3: Option<&ResolvedS3>) -> Result<()> {
    let mut parts = vec!["TYPE s3".to_string()];
    let s3 = s3.cloned().unwrap_or(ResolvedS3 {
        region: None,
        endpoint: None,
        url_style: None,
        key_id: None,
        secret: None,
        use_ssl: None,
    });

    match (&s3.key_id, &s3.secret) {
        (Some(k), Some(s)) => {
            parts.push(format!("KEY_ID '{}'", esc(k)));
            parts.push(format!("SECRET '{}'", esc(s)));
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

    conn.execute_batch(&format!(
        "CREATE OR REPLACE SECRET __cell_s3 ({});",
        parts.join(", ")
    ))
    .context("creating S3 secret")?;
    Ok(())
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

    // ADR 0005 §3: the most expensive flag this engine has must never be
    // silent. Announce before binding, and warn (rather than silently
    // no-op) when the flag has nothing to do.
    if opts.full_refresh {
        if incremental_count > 0 {
            tracing::info!(
                "full refresh: re-reading {incremental_count} incremental sources from zero, \
                 rewriting watermarks"
            );
        } else {
            tracing::warn!(
                "--full-refresh has no effect: this cell declares no incremental sources"
            );
        }
    }
    if opts.verify_replay && incremental_count == 0 {
        tracing::warn!("--verify-replay has no effect: this cell declares no incremental sources");
    }

    // Sources are session-local TEMP VIEWs: visible to transforms, never committed
    // to the catalog.
    connectors::prepare(&cell.sources, &cell.dir)?;
    let mut advances: Vec<WatermarkAdvance> = Vec::new();
    for (i, (name, src)) in cell.sources.iter().enumerate() {
        if let Some(adv) = bind_source(
            &cell.conn,
            i,
            name,
            src,
            &cell.dir,
            cell.s3.as_ref(),
            &cell.scratch,
            opts.full_refresh,
        )? {
            advances.push(adv);
        }
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
    for t in &cell.def.transforms {
        let sql_path = cell.dir.join(t);
        let sql = std::fs::read_to_string(&sql_path)
            .with_context(|| format!("reading transform {}", sql_path.display()))?;
        tracing::info!(transform = %t, "executing");
        cell.conn
            .execute_batch(&sql)
            .with_context(|| format!("executing transform {t}"))?;
    }
    // Watermarks persist inside the same transaction as the data they
    // account for (ADR 0005 §1): atomic with the snapshot, before COMMIT.
    persist_watermarks(&cell.conn, &advances, opts.full_refresh)?;
    cell.conn
        .execute_batch("COMMIT")
        .context("commit snapshot")?;

    if let Some(before) = &shrink_before {
        let after = snapshot_table_counts(&cell.conn)?;
        for (table, before_count, after_count) in shrunk_tables(before, &after) {
            tracing::warn!(
                table = %table, before = before_count, after = after_count,
                "table shrank during a run with incremental sources — likely cause: a \
                 `CREATE OR REPLACE` rebuilt this table from an incremental source view \
                 instead of merging into it (see docs: incremental)"
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
        verify_replay(&cell)?;
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
    Ok(())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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

/// Bind one source as a TEMP VIEW. Raw sources read a path directly; cell sources
/// attach another cell's DuckLake read-only and read its table by name (optionally
/// pinned to a snapshot) — composing through the catalog, not raw files. Returns
/// the source's watermark advance when it is an incremental connection source
/// (`None` for every other arm) — `run` threads the collected advances across
/// the pre-BEGIN/inside-the-transaction boundary (ADR 0005 §1).
#[allow(clippy::too_many_arguments)]
fn bind_source(
    conn: &Connection,
    idx: usize,
    name: &str,
    src: &ResolvedSource,
    dir: &Path,
    s3: Option<&ResolvedS3>,
    scratch: &Path,
    full_refresh: bool,
) -> Result<Option<WatermarkAdvance>> {
    let view = name.replace('"', "\"\"");
    match src {
        ResolvedSource::Raw(uri) => {
            let resolved = resolve_source_uri(uri, dir);
            conn.execute_batch(&format!(
                "CREATE OR REPLACE TEMP VIEW \"{view}\" AS SELECT * FROM '{}';",
                esc(&resolved)
            ))
            .with_context(|| format!("binding source '{name}' -> {resolved}"))?;
            tracing::info!(source = %name, uri = %resolved, "bound raw source");
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
                    let store = Store::for_storage(storage, s3)?;
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
        }
        ResolvedSource::Connection {
            connection,
            config,
            table,
            incremental,
        } => {
            let ty = config.type_name();
            // Alias keyed on the connection name + ATTACH IF NOT EXISTS: a
            // connection shared by several sources attaches once. Qualify first:
            // it's pure validation, and INSTALL/ATTACH may hit the network.
            let alias = format!("__conn_{connection}");
            let qualified = config.qualify(&alias, table)?;
            conn.execute_batch(config.install_load_sql())
                .with_context(|| format!("loading DuckDB '{ty}' extension"))?;
            conn.execute_batch(&config.attach_sql(&alias))
                .with_context(|| format!("attaching connection '{connection}' ({ty})"))?;

            match incremental {
                None => {
                    conn.execute_batch(&format!(
                        "CREATE OR REPLACE TEMP VIEW \"{view}\" AS SELECT * FROM {qualified};"
                    ))
                    .with_context(|| format!("binding connection source '{name}' -> {table}"))?;
                    tracing::info!(source = %name, connection = %connection, table = %table, "bound connection source");
                }
                Some(inc) => {
                    let adv = stage_incremental(
                        conn,
                        idx,
                        name,
                        &view,
                        table,
                        &qualified,
                        inc,
                        full_refresh,
                    )?;
                    return Ok(Some(adv));
                }
            }
        }
    }
    Ok(None)
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
#[derive(Debug, Clone, PartialEq, Eq)]
enum MarkValue {
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
    fn as_literal(&self) -> String {
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
    inc: &ResolvedIncremental,
    full_refresh: bool,
) -> Result<WatermarkAdvance> {
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

    let cq = inc.cursor.replace('"', "\"\"");
    let stage_sql = match &mark {
        Some(m) => format!(
            "CREATE TEMP TABLE __delta_{idx} AS SELECT * FROM {qualified} WHERE \"{cq}\" > {};",
            m.as_literal()
        ),
        None => format!("CREATE TEMP TABLE __delta_{idx} AS SELECT * FROM {qualified};"),
    };
    conn.execute_batch(&stage_sql)
        .with_context(|| format!("staging incremental delta for source '{name}'"))?;
    conn.execute_batch(&format!(
        "CREATE OR REPLACE TEMP VIEW \"{view}\" AS SELECT * FROM __delta_{idx};"
    ))
    .with_context(|| format!("binding incremental source '{name}' -> {table}"))?;

    let (staged_rows, new_max) = compute_advance(conn, idx, &inc.cursor, ty)?;

    match &mark {
        Some(m) => tracing::info!(
            source = %name, staged_rows, watermark = %m,
            "staged delta past watermark"
        ),
        None => tracing::info!(
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
docs/incremental.md.";

/// `--verify-replay` (ADR 0005 §2 item 1): re-run the transform sequence
/// against the identical staged delta inside a transaction it then rolls
/// back, and fail if any output table's contents changed. Detects
/// DUPLICATION (a plain `INSERT ... SELECT`); truncation is structurally
/// idempotent and therefore invisible here (see the shrink detector).
fn verify_replay(cell: &Cell) -> Result<()> {
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
        for t in &cell.def.transforms {
            let sql_path = cell.dir.join(t);
            let sql = std::fs::read_to_string(&sql_path)
                .with_context(|| format!("reading transform {}", sql_path.display()))?;
            cell.conn
                .execute_batch(&sql)
                .with_context(|| format!("re-executing transform {t} for --verify-replay"))?;
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

fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

/// Resolve the storage URI. Local relative paths are made absolute against the
/// cell directory and created; remote URIs pass through untouched.
fn resolve_storage(s: &str, dir: &Path) -> Result<String> {
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
fn resolve_catalog(s: &str, dir: &Path) -> Result<String> {
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

    #[test]
    fn stage_incremental_bootstraps_then_stages_only_the_delta_on_the_next_run() {
        let (conn, _dir) = probe_attach("stage-two-run");
        seed_warehouse(
            &conn,
            &[(1, "2026-01-01 00:00:00"), (2, "2026-01-02 00:00:00")],
        );
        let inc = ts_incremental();

        // Run 1: no watermark yet -> bootstrap, unfiltered, both rows.
        let adv1 = stage_incremental(
            &conn,
            0,
            "events",
            "events",
            "analytics.events",
            "\"warehouse\"",
            &inc,
            false,
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
            &inc,
            false,
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
        let adv1 = stage_incremental(
            &conn,
            0,
            "events",
            "events",
            "analytics.events",
            "\"warehouse\"",
            &inc,
            false,
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
            &inc,
            false,
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
            &inc,
            true,
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
        let adv = stage_incremental(
            &conn,
            0,
            "events",
            "events",
            "analytics.events",
            "\"warehouse\"",
            &inc,
            false,
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
        let adv1 = stage_incremental(
            &conn,
            0,
            "events",
            "events",
            "analytics.events",
            "\"warehouse\"",
            &inc,
            false,
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

    /// A minimal cell (no sources; just transforms) to drive `verify_replay`
    /// directly, mirroring what `run` does inside the transaction.
    fn replay_test_cell(tag: &str, transforms: &[(&str, &str)]) -> Cell {
        let dir = probe_scratch_dir(tag);
        std::fs::create_dir_all(dir.join("sql")).unwrap();
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        let list = transforms
            .iter()
            .map(|(f, _)| format!("  - sql/{f}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            dir.join("cell.yaml"),
            format!("cell: t\ntransforms:\n{list}\ninterface: []\n"),
        )
        .unwrap();
        std::fs::write(
            dir.join("profiles/local.yaml"),
            "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
        )
        .unwrap();
        for (f, sql) in transforms {
            std::fs::write(dir.join("sql").join(f), sql).unwrap();
        }
        let cell = open(&dir.join("cell.yaml"), "local", false).expect("open test cell");
        cell.conn.execute_batch("BEGIN").unwrap();
        for t in &cell.def.transforms {
            let sql = std::fs::read_to_string(cell.dir.join(t)).unwrap();
            cell.conn.execute_batch(&sql).unwrap();
        }
        cell.conn.execute_batch("COMMIT").unwrap();
        cell
    }

    #[test]
    fn verify_replay_passes_on_an_anti_join_transform() {
        let cell = replay_test_cell(
            "replay-antijoin",
            &[(
                "t.sql",
                "CREATE TABLE IF NOT EXISTS fct(id INTEGER);\n\
                 INSERT INTO fct SELECT id FROM range(5) r(id) \
                 EXCEPT SELECT id FROM fct;",
            )],
        );
        verify_replay(&cell).expect("anti-join transform must be replay-safe");
    }

    #[test]
    fn verify_replay_fails_on_a_plain_insert_that_duplicates() {
        let cell = replay_test_cell(
            "replay-duplicate",
            &[(
                "t.sql",
                "CREATE TABLE IF NOT EXISTS fct(id INTEGER);\n\
                 INSERT INTO fct SELECT id FROM range(5) r(id);",
            )],
        );
        let err = verify_replay(&cell).unwrap_err().to_string();
        assert!(err.contains("replay-unsafe transform"), "got: {err}");
        assert!(err.contains("'fct'"), "got: {err}");
        assert!(err.contains("(5 -> 10 rows)"), "got: {err}");
        assert!(err.contains("never `CREATE OR REPLACE`"), "got: {err}");
    }

    #[test]
    fn verify_replay_does_not_fail_on_a_deterministic_create_or_replace_truncation() {
        // Truncation is idempotent and structurally invisible to a
        // pass1-vs-pass2 comparison (ADR 0005 §2) — --verify-replay must NOT
        // flag it; the shrink warning (`shrunk_tables`, tested above) is the
        // detector for this shape instead.
        let cell = replay_test_cell(
            "replay-truncation",
            &[(
                "t.sql",
                "CREATE OR REPLACE TABLE fct AS SELECT id FROM range(5) r(id);",
            )],
        );
        verify_replay(&cell)
            .expect("a deterministic CREATE OR REPLACE must not fail verify-replay");
    }

    #[test]
    fn verify_replay_wraps_a_non_rerunnable_transform_with_a_named_cause() {
        // A plain `CREATE TABLE` (no OR REPLACE / IF NOT EXISTS) errors on
        // its second execution — a transform that cannot re-run is broken
        // independent of incremental sources.
        let cell = replay_test_cell(
            "replay-nonrerunnable",
            &[(
                "t.sql",
                "CREATE TABLE fct AS SELECT id FROM range(5) r(id);",
            )],
        );
        let err = verify_replay(&cell).unwrap_err().to_string();
        assert!(err.contains("must be re-runnable"), "got: {err}");
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
}
