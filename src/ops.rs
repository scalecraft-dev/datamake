//! Operational commands over the published-catalog layout (ADR 0004):
//! `datamk status` (the read side that makes operations legible) and
//! `datamk rollback` (repoint `LATEST`, guarded so a supported route's pin can
//! never be stranded). Both need only bucket credentials — no cluster access,
//! the same "datamk references infrastructure" posture as deploy.
//!
//! ADR 0005 §4 ("Making the state legible") adds a watermark narration to
//! both: `status` shows what the next run will pick up per incremental
//! source, `rollback` shows the watermark rewind it is about to cause. Both
//! read `__datamk_watermarks` out of a downloaded artifact copy — display
//! only, best-effort (a read failure degrades to a warning, never fails the
//! command outright; see `read_watermark_rows`/`read_watermarks_for_execution`).

use anyhow::{bail, Context, Result};
use duckdb::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config;
use crate::engine;
use crate::store::{execution_key, latest_content, Store, LATEST_KEY};

/// Load the profile and build the cell's store handle; both commands only make
/// sense for published-artifact profiles.
fn published_store(file: &Path, profile: &str) -> Result<(config::LoadedCell, Arc<Store>)> {
    let loaded = config::load(file, profile)?;
    if let Some(c) = &loaded.bindings.catalog {
        bail!(
            "profiles/{profile}.yaml sets `catalog:` ({c}) — direct-attach mode has no \
             published executions. `status`/`rollback` apply to published-artifact profiles \
             (no `catalog:`; ADR 0004)."
        );
    }
    let store = Store::for_storage(
        &loaded.bindings.storage,
        loaded.bindings.s3.as_ref(),
        loaded.bindings.gcs.as_ref(),
    )?;
    Ok((loaded, Arc::new(store)))
}

/// Print the published range, the `LATEST` pointer, and its age.
pub fn status(file: &Path, profile: &str) -> Result<()> {
    let (loaded, store) = published_store(file, profile)?;
    let executions = store.list_executions()?;
    let latest = store.latest()?;
    let modified = store.last_modified(LATEST_KEY)?;

    println!("cell: {}   profile: {}", loaded.def.cell, profile);
    println!("storage: {}", loaded.bindings.storage);
    match (executions.first(), executions.last()) {
        (Some(first), Some(last)) => println!(
            "executions: {first}..{last} published ({} artifact{})",
            executions.len(),
            if executions.len() == 1 { "" } else { "s" }
        ),
        _ => println!("executions: none published"),
    }
    match latest {
        Some(n) => match modified {
            Some(ts) => println!("LATEST -> {n}   (pointer written {ts})"),
            None => println!("LATEST -> {n}"),
        },
        None => println!("LATEST: absent (no execution published yet — run `datamk run`)"),
    }

    // Persistent run logs + a published run-summary: a compact narration of
    // LATEST's run.json, if one exists. Observability only — never a
    // substitute for the watermark block below, which stays sourced from
    // the catalog itself.
    if let Some(n) = latest {
        print_last_run_summary(&store, n);
    }

    // ADR 0005 §4: only when the cell declares incremental sources AND there
    // is a LATEST to read them from — a cell with none must not download
    // anything, and there's nothing to read when nothing has been published.
    let declared = declared_incremental_sources(&loaded.bindings);
    if !declared.is_empty() {
        if let Some(n) = latest {
            print_status_watermarks(&store, &loaded.bindings, n, &declared);
        }
    }
    Ok(())
}

/// A compact narration of `execution`'s published run summary
/// (`engine::run_summary::RunSummary`, `<N>.run.json`), if one exists and
/// parses. Absent (an artifact from before this feature, or a summary
/// write that itself warned and skipped) or unreadable (corrupt/foreign
/// JSON at that key) both silently no-op — this is best-effort
/// observability layered on top of `status`, never something `status`
/// depends on to be useful.
fn print_last_run_summary(store: &Store, execution: u64) {
    let Ok(Some(bytes)) = store.get(&crate::store::run_summary_key(execution)) else {
        return;
    };
    let Ok(summary) = serde_json::from_slice::<engine::run_summary::RunSummary>(&bytes) else {
        return;
    };
    for line in last_run_summary_lines(&summary) {
        println!("{line}");
    }
}

/// Pure line-building for `print_last_run_summary`, split out for testing
/// (mirrors `format_status_lines`/`format_rollback_lines` below): a header
/// (verify outcome, transform count/total duration) followed by one line
/// per source that actually staged something — raw/cell/table sources with
/// nothing to narrate are skipped, not shown with a fabricated zero.
fn last_run_summary_lines(summary: &engine::run_summary::RunSummary) -> Vec<String> {
    let total_ms: u64 = summary.transforms.iter().map(|t| t.duration_ms).sum();
    let mut lines = vec![format!(
        "last run (execution {}): verify {}, {} transform{} in {} ms",
        summary.execution,
        summary.verify_outcome,
        summary.transforms.len(),
        if summary.transforms.len() == 1 {
            ""
        } else {
            "s"
        },
        group_thousands(total_ms as i64)
    )];
    for s in &summary.sources {
        let Some(rows) = s.staged_rows else {
            continue; // nothing staged to narrate (raw/cell/table sources)
        };
        let bytes_part = s
            .bytes_scanned
            .map(|b| format!(" ({} bytes scanned)", group_thousands(b)))
            .unwrap_or_default();
        lines.push(format!(
            "  {}: {} rows staged{bytes_part}",
            s.name,
            group_thousands(rows as i64)
        ));
    }
    lines
}

/// Repoint `LATEST` to an earlier (or explicit) execution. Refuses a target
/// that doesn't exist or that lacks any currently pinned snapshot — the guard
/// that keeps a supported route from 500-ing on `AT (VERSION => <pin>)`
/// against an artifact from before the pin (ADR 0004 §9).
pub fn rollback(file: &Path, profile: &str, execution: Option<u64>) -> Result<()> {
    let (loaded, store) = published_store(file, profile)?;
    let executions = store.list_executions()?;
    let current = store
        .latest()?
        .context("no LATEST pointer — nothing has been published, nothing to roll back")?;

    let target = match execution {
        Some(n) => n,
        // Default: the newest published artifact before the one LATEST names
        // (execution numbers can have gaps — dead branches from prior rollbacks).
        None => *executions
            .iter()
            .rfind(|&&n| n < current)
            .with_context(|| {
                format!("LATEST -> {current} and no earlier artifact exists to roll back to")
            })?,
    };

    if target == current {
        bail!("LATEST already points at execution {current}; nothing to do");
    }
    if !executions.contains(&target) {
        let range = match (executions.first(), executions.last()) {
            (Some(f), Some(l)) => format!("{f}..{l} ({} published)", executions.len()),
            _ => "none published".to_string(),
        };
        bail!("execution {target} does not exist; available: {range}");
    }

    let pins = load_pins(&loaded.dir);
    let declared = declared_incremental_sources(&loaded.bindings);

    // The pin guard and the watermark narration both need the *target*
    // artifact; download it once and share it (ADR 0005 §4) instead of
    // fetching it twice.
    let target_scratch = std::env::temp_dir().join(format!(
        "datamk-rollback-target-{}-{}",
        std::process::id(),
        target
    ));
    let mut target_local: Option<PathBuf> = None;
    if !pins.is_empty() || !declared.is_empty() {
        match store.download_execution(target, &target_scratch) {
            Ok(local) => target_local = Some(local),
            Err(e) if pins.is_empty() => {
                // Only the watermark narration needed this download; that's
                // a display concern, not a guard — degrade, don't fail.
                eprintln!("warning: could not read watermarks from execution {target}: {e}");
            }
            Err(e) => {
                // The pin guard needs this download; unchanged hard-fail.
                let _ = std::fs::remove_dir_all(&target_scratch);
                return Err(e.context(format!("downloading execution {target} for the pin guard")));
            }
        }
    }

    if !pins.is_empty() {
        // `target_local` is guaranteed `Some` here: the only way to reach
        // this arm with pins non-empty and `target_local` still `None` is the
        // hard-fail branch above, which already returned.
        let local = target_local
            .as_ref()
            .expect("target artifact downloaded above when pins are present");
        if let Err(e) = check_pins_present(local, &loaded.bindings.storage, &loaded, &pins, target)
        {
            let _ = std::fs::remove_dir_all(&target_scratch);
            return Err(e);
        }
    }

    store.put(LATEST_KEY, latest_content(target).into_bytes())?;
    println!("LATEST {current} -> {target}");

    if !declared.is_empty() {
        print_rollback_watermarks(
            &store,
            &loaded,
            current,
            target,
            target_local.as_deref(),
            &declared,
        );
    }

    println!(
        "note: the next scheduled execution builds on execution {target}'s lineage and \
         publishes a fresh number; suspend the Builder CronJob if you want the world frozen here."
    );

    let _ = std::fs::remove_dir_all(&target_scratch);
    Ok(())
}

/// Pinned snapshot ids from the release manifest, if any.
fn load_pins(dir: &Path) -> Vec<(String, i64)> {
    let path = dir.join(".cell").join("published.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<crate::manifest::Published>(&raw).ok())
        .map(|p| p.routes.into_iter().collect())
        .unwrap_or_default()
}

fn check_pins_present(
    local: &Path,
    storage: &str,
    loaded: &config::LoadedCell,
    pins: &[(String, i64)],
    target: u64,
) -> Result<()> {
    let conn = engine::open_artifact(
        local,
        storage,
        loaded.bindings.s3.as_ref(),
        loaded.bindings.gcs.as_ref(),
    )?;
    let mut stmt = conn
        .prepare("SELECT snapshot_id FROM ducklake_snapshots('lake')")
        .context("querying target artifact's snapshots")?;
    let snapshots: Vec<i64> = stmt
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<std::result::Result<_, _>>()?;
    for (route, pin) in pins {
        if !snapshots.contains(pin) {
            bail!(
                "rollback to execution {target} refused: supported route '{route}' is pinned to \
                 snapshot {pin} (release manifest), which does not exist in that artifact — the \
                 route would 500 on every request.\n\
                 Roll back to an artifact that contains snapshot {pin}, or re-run `datamk \
                 release` against the rolled-back state first (a reviewed re-pin)."
            );
        }
    }
    Ok(())
}

// --- ADR 0005 §4: watermark narration -------------------------------------

/// `(source name, declared cursor column)` for every `connection` source that
/// declares `incremental:`, in `cell.yaml`'s declared order (contract order —
/// `ResolvedBindings::sources` is an `IndexMap`). The cursor here is the
/// *declared* one, expanded but not bind-time-validated; it is only ever
/// shown for a source that has no watermark row yet (bootstrap), where there
/// is no persisted row to read it from instead.
fn declared_incremental_sources(bindings: &config::ResolvedBindings) -> Vec<(String, String)> {
    bindings
        .sources
        .iter()
        .filter_map(|(name, src)| match src {
            config::ResolvedSource::Connection {
                incremental: Some(inc),
                ..
            } => Some((name.clone(), inc.cursor.clone())),
            _ => None,
        })
        .collect()
}

/// One row of `__datamk_watermarks`, as read back (display only — never fed
/// into a predicate, unlike the engine's `MarkValue`).
struct RawWatermarkRow {
    source: String,
    cursor_column: String,
    mark_ts: Option<String>,
    mark_date: Option<String>,
    mark_int: Option<i64>,
    last_delta_rows: i64,
}

/// Read every row of `__datamk_watermarks` from an already-attached artifact
/// connection. `Ok(vec![])` when the table doesn't exist at all (an artifact
/// from before ADR 0005, or one that never staged an incremental source) —
/// indistinguishable, from the display side, from "no sources have run yet".
/// Duplicate rows fail loud with the engine's own corrupt-state error (R3):
/// display robustness matters less than never reporting a fabricated mark.
fn read_watermark_rows(conn: &Connection) -> Result<Vec<RawWatermarkRow>> {
    if !engine::watermark_table_exists(conn)? {
        return Ok(Vec::new());
    }
    engine::check_watermark_duplicates(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT source, cursor_column, mark_ts::VARCHAR, mark_date::VARCHAR, mark_int, \
             last_delta_rows FROM __datamk_watermarks ORDER BY source",
        )
        .context("preparing __datamk_watermarks read")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(RawWatermarkRow {
                source: r.get(0)?,
                cursor_column: r.get(1)?,
                mark_ts: r.get(2)?,
                mark_date: r.get(3)?,
                mark_int: r.get(4)?,
                last_delta_rows: r.get(5)?,
            })
        })
        .context("querying __datamk_watermarks")?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .context("reading __datamk_watermarks rows")
}

/// Download execution `n`, attach it read-only, and read its watermark rows —
/// the scratch dir is removed before returning either way. The one place that
/// downloads an artifact purely to *display* its watermark state (`status`'s
/// LATEST, `rollback`'s current execution); a failure anywhere in this chain
/// is the caller's to degrade, not to propagate as a command failure.
fn read_watermarks_for_execution(
    store: &Store,
    storage: &str,
    s3: Option<&config::ResolvedS3>,
    gcs: Option<&config::ResolvedGcs>,
    execution: u64,
) -> Result<Vec<RawWatermarkRow>> {
    let scratch = std::env::temp_dir().join(format!(
        "datamk-watermarks-{}-{}",
        std::process::id(),
        execution
    ));
    let result = store
        .download_execution(execution, &scratch)
        .and_then(|local| {
            let conn = engine::open_artifact(&local, storage, s3, gcs)?;
            read_watermark_rows(&conn)
        });
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn print_status_watermarks(
    store: &Store,
    bindings: &config::ResolvedBindings,
    execution: u64,
    declared: &[(String, String)],
) {
    match read_watermarks_for_execution(
        store,
        &bindings.storage,
        bindings.s3.as_ref(),
        bindings.gcs.as_ref(),
        execution,
    ) {
        Ok(rows) => {
            let by_source: HashMap<&str, &RawWatermarkRow> =
                rows.iter().map(|r| (r.source.as_str(), r)).collect();
            let items: Vec<SourceWatermark> = declared
                .iter()
                .map(|(name, cursor)| match by_source.get(name.as_str()) {
                    Some(row) => SourceWatermark {
                        name: name.clone(),
                        cursor_column: row.cursor_column.clone(),
                        state: WatermarkState::Present {
                            mark: render_mark(row),
                            last_delta_rows: row.last_delta_rows,
                        },
                    },
                    None => SourceWatermark {
                        name: name.clone(),
                        cursor_column: cursor.clone(),
                        state: WatermarkState::Absent,
                    },
                })
                .collect();
            println!("watermarks (at LATEST):");
            for line in format_status_lines(&items) {
                println!("{line}");
            }
        }
        Err(e) => {
            eprintln!("warning: could not read watermarks from execution {execution}: {e}")
        }
    }
}

fn print_rollback_watermarks(
    store: &Store,
    loaded: &config::LoadedCell,
    current: u64,
    target: u64,
    target_local: Option<&Path>,
    declared: &[(String, String)],
) {
    let current_rows = match read_watermarks_for_execution(
        store,
        &loaded.bindings.storage,
        loaded.bindings.s3.as_ref(),
        loaded.bindings.gcs.as_ref(),
        current,
    ) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("warning: could not read watermarks from execution {current}: {e}");
            return;
        }
    };

    let target_rows = match target_local {
        Some(local) => {
            let opened = engine::open_artifact(
                local,
                &loaded.bindings.storage,
                loaded.bindings.s3.as_ref(),
                loaded.bindings.gcs.as_ref(),
            )
            .and_then(|conn| read_watermark_rows(&conn));
            match opened {
                Ok(rows) => rows,
                Err(e) => {
                    eprintln!("warning: could not read watermarks from execution {target}: {e}");
                    return;
                }
            }
        }
        // The download already failed and warned (see `rollback`'s
        // `need_target_download` gate — it's only ever `None` here because
        // that attempt errored).
        None => return,
    };

    let items = build_rollback_changes(declared, &current_rows, &target_rows);
    if items.is_empty() {
        return;
    }
    for line in format_rollback_lines(&items, target) {
        println!("{line}");
    }
}

/// Render a `RawWatermarkRow`'s single non-NULL mark column (engine
/// invariant: exactly one of `mark_ts`/`mark_date`/`mark_int` is non-NULL per
/// row) at full precision. Timestamps are cosmetically reformatted to ISO-8601
/// (`T` separator, `Z` for a `+00` offset) — the value itself is exactly what
/// the engine wrote, never re-derived.
fn render_mark(row: &RawWatermarkRow) -> String {
    if let Some(ts) = &row.mark_ts {
        format_timestamp_mark(ts)
    } else if let Some(d) = &row.mark_date {
        d.clone()
    } else if let Some(n) = row.mark_int {
        n.to_string()
    } else {
        // Unreachable under the engine's invariant; empty rather than panic —
        // this is a display path, not a correctness gate (that's
        // `check_watermark_duplicates`/the engine's own writes).
        String::new()
    }
}

/// DuckDB's `TIMESTAMPTZ::VARCHAR` renders `2026-07-04 11:58:00+00` (space
/// separator, no colon in the offset); reformat to `2026-07-04T11:58:00Z` for
/// a UTC offset, otherwise just swap the separator and leave the offset as
/// DuckDB rendered it.
fn format_timestamp_mark(raw: &str) -> String {
    let iso = raw.replacen(' ', "T", 1);
    match iso.strip_suffix("+00") {
        Some(stripped) => format!("{stripped}Z"),
        None => iso,
    }
}

/// Thousands-grouped decimal rendering for interactive `println!`/`eprintln!`
/// output (`status`'s "+N rows last run"; the shrink-detector end-of-run
/// summary in `engine::run`). `pub(crate)` so `engine` can share it rather
/// than growing a second copy.
pub(crate) fn group_thousands(n: i64) -> String {
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let grouped: String = grouped.chars().rev().collect();
    if neg {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// `status`'s per-source watermark: the declared cursor plus either the
/// current mark and last delta size, or "no watermark row yet" (bootstrap).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceWatermark {
    name: String,
    cursor_column: String,
    state: WatermarkState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WatermarkState {
    Present { mark: String, last_delta_rows: i64 },
    Absent,
}

fn delta_suffix(last_delta_rows: i64) -> String {
    if last_delta_rows == 0 {
        "(no new rows last run)".to_string()
    } else {
        format!("(+{} rows last run)", group_thousands(last_delta_rows))
    }
}

/// Pure formatter (testable without a store/connection): one line per source,
/// name and `cursor=<column>` each padded to the widest entry, three-space
/// gaps between columns (house style).
fn format_status_lines(rows: &[SourceWatermark]) -> Vec<String> {
    let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(0) + 3;
    let cursor_w = rows
        .iter()
        .map(|r| r.cursor_column.len())
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|r| {
            let suffix = match &r.state {
                WatermarkState::Present {
                    mark,
                    last_delta_rows,
                } => format!("mark={mark}   {}", delta_suffix(*last_delta_rows)),
                WatermarkState::Absent => "absent — next run bootstraps a full scan".to_string(),
            };
            format!(
                "  {name:<name_w$}cursor={cursor:<cursor_w$}   {suffix}",
                name = r.name,
                cursor = r.cursor_column,
            )
        })
        .collect()
}

/// `rollback`'s per-source watermark movement: a real rewind (both artifacts
/// have a row, marks differ), or the target predating incremental loading for
/// this source entirely. Absent in both, or present with an identical mark in
/// both (a repoint without any watermark movement), narrate nothing — see
/// `build_rollback_changes`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RollbackChange {
    Rewind {
        cursor: String,
        from: String,
        to: String,
    },
    AbsentInTarget,
}

/// Pure diff (testable without a store/connection): pair each declared
/// source's current-artifact row against its target-artifact row and decide
/// what, if anything, to narrate.
///
/// - Absent in both -> nothing (never had a watermark; still doesn't).
/// - Present in both, same mark -> nothing (repointing without watermark
///   movement needs no narration).
/// - Present in both, different mark -> `Rewind`.
/// - Present in current, absent in target -> `AbsentInTarget` (the target
///   artifact predates this source's incremental loading).
/// - Absent in current, present in target (a forward `rollback` to a *later*
///   execution than LATEST, via an explicit `--execution`): not one of the
///   ADR's named cases; narrating a "rewind" backwards would be wrong since
///   nothing is rewinding, so this prints nothing rather than invent wording.
fn build_rollback_changes(
    declared: &[(String, String)],
    current_rows: &[RawWatermarkRow],
    target_rows: &[RawWatermarkRow],
) -> Vec<(String, RollbackChange)> {
    let current_by_source: HashMap<&str, &RawWatermarkRow> = current_rows
        .iter()
        .map(|r| (r.source.as_str(), r))
        .collect();
    let target_by_source: HashMap<&str, &RawWatermarkRow> =
        target_rows.iter().map(|r| (r.source.as_str(), r)).collect();

    declared
        .iter()
        .filter_map(|(name, _cursor)| {
            let cur = current_by_source.get(name.as_str());
            let tgt = target_by_source.get(name.as_str());
            match (cur, tgt) {
                (Some(c), Some(t)) => {
                    let from = render_mark(c);
                    let to = render_mark(t);
                    if from == to {
                        None
                    } else {
                        Some((
                            name.clone(),
                            RollbackChange::Rewind {
                                cursor: t.cursor_column.clone(),
                                from,
                                to,
                            },
                        ))
                    }
                }
                (Some(_), None) => Some((name.clone(), RollbackChange::AbsentInTarget)),
                (None, Some(_)) | (None, None) => None,
            }
        })
        .collect()
}

/// Pure formatter (testable without a store/connection): two lines per
/// narrated source — the movement, then the continuation line describing what
/// the next run does about it — both indented to line up under the source
/// name column (house style: three-space gaps).
fn format_rollback_lines(items: &[(String, RollbackChange)], target_execution: u64) -> Vec<String> {
    let name_w = items.iter().map(|(n, _)| n.len()).max().unwrap_or(0) + 3;
    let continuation_indent = " ".repeat(name_w + 2);
    items
        .iter()
        .flat_map(|(name, change)| match change {
            RollbackChange::Rewind { cursor, from, to } => vec![
                format!("  {name:<name_w$}watermark rewinds {cursor} {from} -> {to};"),
                format!("{continuation_indent}next run re-ingests rows where {cursor} > {to}"),
            ],
            RollbackChange::AbsentInTarget => vec![
                format!(
                    "  {name:<name_w$}watermark rewinds to absent (execution {target_execution} \
                     predates incremental loading);"
                ),
                format!("{continuation_indent}next run bootstraps a full scan"),
            ],
        })
        .collect()
}

/// `datamk attach`: print ready-to-run SQL that attaches the cell's catalog
/// in DuckDB, read-only. stdout carries ONLY SQL — one statement per line —
/// so `duckdb -c "$(datamk attach ...) SELECT ..."` composes; resolution
/// notes go to stderr. The attach mirrors the engine's own (same resolved
/// paths, same DATA_PATH, same secret options via `engine::s3_secret_options`)
/// so the printed recipe can never drift from what datamk itself does.
///
/// By default the recipe is a stateless, portable reference — no local state,
/// runnable on any host with credentials. The one shape that can't satisfy
/// that is a native-GCS-extension profile (the extension cannot ATTACH a
/// remote catalog file), which refuses by default and requires `--download`:
/// an explicit, machine-local materialization under `<cell>/.cell/attach/`.
pub fn attach(file: &Path, profile: &str, execution: Option<u64>, download: bool) -> Result<()> {
    let loaded = config::load(file, profile)?;
    let alias = sanitize_ident(&loaded.def.cell);
    let storage = engine::resolve_storage(&loaded.bindings.storage, &loaded.dir)?;

    // The only profile shape --download applies to: published-artifact mode,
    // gs:// storage, native extension configured. Everything else attaches
    // its catalog directly (remote via httpfs, or the local direct-attach
    // file) — reject the flag there so the surface stays honest.
    let native_gcs = loaded.bindings.catalog.is_none()
        && config::is_gcs(&storage)
        && loaded
            .bindings
            .gcs
            .as_ref()
            .is_some_and(|g| g.extension.is_some());
    if download && !native_gcs {
        bail!(
            "--download only applies to native-GCS-extension profiles (gcs.extension). Profile \
             '{profile}' attaches its catalog directly — drop --download."
        );
    }

    // Direct-attach (local dev) profiles: a `catalog:` and no published
    // executions — attach that catalog exactly as the engine would.
    if let Some(c) = &loaded.bindings.catalog {
        if let Some(n) = execution {
            bail!(
                "profiles/{profile}.yaml sets `catalog:` (direct-attach mode) — there are no \
                 published executions to pin, so --execution {n} does not apply here"
            );
        }
        let catalog = engine::resolve_catalog(c, &loaded.dir)?;
        if !config::is_metadata_db_catalog(&catalog) && !Path::new(&catalog).exists() {
            bail!(
                "no catalog at {catalog} — run `datamk run -f {} -p {profile}` first",
                file.display()
            );
        }
        print_attach_sql(&alias, &catalog, &storage, &loaded.bindings, false)?;
        eprintln!(
            "attach: cell '{}' profile '{profile}' (direct-attach catalog)",
            loaded.def.cell
        );
        return Ok(());
    }

    // Published-artifact profiles (ADR 0004): resolve the artifact to attach.
    if !config::is_remote(&storage) {
        bail!(
            "the profile has no `catalog:` (published-artifact mode), but storage `{storage}` \
             is not an object store — nothing published to attach. For local development set \
             `catalog:` (e.g. ./.cell/catalog.ducklake)."
        );
    }
    let store = Store::for_storage(
        &storage,
        loaded.bindings.s3.as_ref(),
        loaded.bindings.gcs.as_ref(),
    )?;
    let n = match execution {
        Some(n) => {
            let executions = store.list_executions()?;
            if !executions.contains(&n) {
                let range = match (executions.first(), executions.last()) {
                    (Some(f), Some(l)) => format!("{f}..{l} ({} published)", executions.len()),
                    _ => "none published".to_string(),
                };
                bail!("execution {n} does not exist; available: {range}");
            }
            eprintln!(
                "attach: pinning execution {n} — a rollback may have retired it; LATEST is the \
                 served view"
            );
            n
        }
        None => store.latest()?.with_context(|| {
            format!(
                "no LATEST pointer under {storage} — nothing published yet; run `datamk run -f \
                 {} -p {profile}` first",
                file.display()
            )
        })?,
    };

    let catalog = format!("{}/{}", storage.trim_end_matches('/'), execution_key(n));
    let data_path = format!("{storage}/data");
    // A native GCS extension can read data files but cannot ATTACH a remote
    // *database* file (httpfs has bespoke attach-over-remote support the
    // extension lacks), so a portable remote reference is physically
    // impossible for this shape. Refuse by default; with --download,
    // materialize a local copy the way the engine does (download + attach
    // local + OVERRIDE_DATA_PATH), reusing an already-fetched artifact —
    // executions are immutable, so an existing copy is always byte-equal.
    let (catalog, override_dp) = if native_gcs {
        if !download {
            bail!(
                "profile '{profile}' uses the native GCS extension (gcs.extension), which \
                 cannot ATTACH a remote catalog file — DuckDB's attach-over-remote is \
                 httpfs-only. Re-run with --download to fetch execution {n} to \
                 .cell/attach/ and attach that local copy, or set gcs.key_id/gcs.secret \
                 (HMAC) to attach gs:// directly."
            );
        }
        let cache = loaded.dir.join(".cell").join("attach");
        let cached = cache.join(crate::store::execution_artifact_name(n));
        let local = if cached.is_file() {
            eprintln!(
                "attach: reusing already-downloaded execution {n} at {} (artifacts are \
                 immutable; delete the file to force a re-fetch)",
                cached.display()
            );
            cached
        } else {
            // Download to a scratch subdir, then rename into place — a crash
            // mid-download must never leave a partial file where the reuse
            // check above would trust it.
            let tmp = cache.join(format!(".tmp-{}", std::process::id()));
            let fetched = store.download_execution(n, &tmp)?;
            std::fs::rename(&fetched, &cached)
                .with_context(|| format!("moving {} into place", fetched.display()))?;
            let _ = std::fs::remove_dir_all(&tmp);
            eprintln!("attach: downloaded execution {n} to {}", cached.display());
            cached
        };
        eprintln!(
            "attach: this recipe attaches a LOCAL snapshot of execution {n} — it is \
             machine-specific (don't share the SQL) and will not track new executions; \
             re-run `datamk attach --download` to refresh"
        );
        (local.to_string_lossy().into_owned(), true)
    } else {
        (catalog, false)
    };
    print_attach_sql(&alias, &catalog, &data_path, &loaded.bindings, override_dp)?;
    eprintln!(
        "attach: cell '{}' profile '{profile}' -> execution {n}{}",
        loaded.def.cell,
        if execution.is_none() { " (LATEST)" } else { "" }
    );
    Ok(())
}

/// The SQL itself (stdout): a namespaced store secret when anything attached
/// is on S3 or GCS (same options the engine registers for itself), then the
/// read-only ATTACH with the engine's explicit DATA_PATH.
fn print_attach_sql(
    alias: &str,
    catalog: &str,
    data_path: &str,
    b: &config::ResolvedBindings,
    override_data_path: bool,
) -> Result<()> {
    let sql = attach_sql(
        alias,
        catalog,
        data_path,
        b.s3.as_ref(),
        b.gcs.as_ref(),
        override_data_path,
    )?;
    if sql.contains("LOAD '") {
        // A native GCS extension is unsigned; plain `duckdb` refuses to LOAD it.
        eprintln!("attach: the SQL loads a native GCS extension — run `duckdb -unsigned`");
    }
    print!("{sql}");
    Ok(())
}

/// Pure builder (testable without a store): one `;`-terminated statement per
/// line, nothing but SQL. Errs only when GCS is attached without the HMAC
/// pair DuckDB needs (the printed recipe would 401 on every read).
fn attach_sql(
    alias: &str,
    catalog: &str,
    data_path: &str,
    s3: Option<&config::ResolvedS3>,
    gcs: Option<&config::ResolvedGcs>,
    override_data_path: bool,
) -> Result<String> {
    let mut out = String::new();
    if config::is_s3(catalog) || config::is_s3(data_path) {
        out.push_str(&format!(
            "CREATE OR REPLACE SECRET datamk_{alias} ({});\n",
            engine::s3_secret_options(s3)
        ));
    }
    if config::is_gcs(catalog) || config::is_gcs(data_path) {
        // Native-extension mode: the recipe must LOAD the extension before
        // the secret. Needs `duckdb -unsigned`; attach() notes that on stderr.
        if let Some(load) = engine::gcs_load_sql(gcs) {
            // httpfs must register BEFORE the native extension: DuckLake
            // autoloads httpfs when it sees a remote DATA_PATH, and whichever
            // gs:// filesystem registers last wins routing. If httpfs lands
            // after the native extension it shadows it, and every data-file
            // read 403s with httpfs's HMAC "No credentials are provided" —
            // environment-dependently (only when httpfs is installed). Load
            // it first, as the engine's own setup does (engine::setup /
            // open_artifact), so the native filesystem registers last.
            out.push_str("INSTALL httpfs;\nLOAD httpfs;\n");
            out.push_str(&load);
            out.push('\n');
        }
        out.push_str(&format!(
            "CREATE OR REPLACE SECRET datamk_{alias}_gcs ({});\n",
            engine::gcs_secret_options(gcs)?
        ));
    }
    // A downloaded artifact copy records whatever DATA_PATH its builder used;
    // the profile's storage is authoritative (engine::open_artifact does the
    // same).
    let odp = if override_data_path {
        ", OVERRIDE_DATA_PATH true"
    } else {
        ""
    };
    out.push_str(&format!(
        "ATTACH 'ducklake:{}' AS {alias} (DATA_PATH '{}', READ_ONLY{odp});\n",
        engine::esc(catalog),
        engine::esc(data_path)
    ));
    Ok(out)
}

/// A cell name as a DuckDB identifier: `weather-kube` -> `weather_kube`.
fn sanitize_ident(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        s.insert(0, '_');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- attach ------------------------------------------------------------

    #[test]
    fn sanitize_ident_maps_cell_names_to_duckdb_identifiers() {
        assert_eq!(sanitize_ident("weather-kube"), "weather_kube");
        assert_eq!(sanitize_ident("orders"), "orders");
        assert_eq!(sanitize_ident("1st.cell"), "_1st_cell");
    }

    #[test]
    fn attach_sql_on_s3_prints_chain_secret_then_pinned_readonly_attach() {
        let s3 = config::ResolvedS3 {
            region: Some("us-west-2".to_string()),
            endpoint: None,
            url_style: None,
            key_id: None,
            secret: None,
            session_token: None,
            use_ssl: None,
        };
        let sql = attach_sql(
            "weather_kube",
            "s3://bkt/cells/weather-kube/catalog/executions/00000008.ducklake",
            "s3://bkt/cells/weather-kube/data",
            Some(&s3),
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            sql,
            "CREATE OR REPLACE SECRET datamk_weather_kube (TYPE s3, PROVIDER credential_chain, \
             REGION 'us-west-2');\n\
             ATTACH 'ducklake:s3://bkt/cells/weather-kube/catalog/executions/00000008.ducklake' \
             AS weather_kube (DATA_PATH 's3://bkt/cells/weather-kube/data', READ_ONLY);\n"
        );
    }

    #[test]
    fn attach_sql_local_catalog_has_no_secret() {
        let sql = attach_sql(
            "orders",
            "/abs/.cell/catalog.ducklake",
            "/abs/.cell/data",
            None,
            None,
            false,
        )
        .unwrap();
        assert!(!sql.contains("CREATE OR REPLACE SECRET"), "got: {sql}");
        assert!(sql.contains("ATTACH 'ducklake:/abs/.cell/catalog.ducklake' AS orders"));
        assert!(sql.contains("READ_ONLY"));
    }

    #[test]
    fn attach_sql_on_gcs_prints_hmac_secret_then_pinned_readonly_attach() {
        let gcs = config::ResolvedGcs {
            credentials: None,
            extension: None,
            key_id: Some("HMACKEY".to_string()),
            secret: Some("HMACSECRET".to_string()),
            endpoint: None,
            use_ssl: None,
        };
        let sql = attach_sql(
            "orders",
            "gs://bkt/cells/orders/catalog/executions/00000003.ducklake",
            "gs://bkt/cells/orders/data",
            None,
            Some(&gcs),
            false,
        )
        .unwrap();
        assert_eq!(
            sql,
            "CREATE OR REPLACE SECRET datamk_orders_gcs (TYPE gcs, KEY_ID 'HMACKEY', \
             SECRET 'HMACSECRET');\n\
             ATTACH 'ducklake:gs://bkt/cells/orders/catalog/executions/00000003.ducklake' \
             AS orders (DATA_PATH 'gs://bkt/cells/orders/data', READ_ONLY);\n"
        );
    }

    #[test]
    fn attach_sql_on_gcs_native_extension_loads_then_creates_gcp_secret() {
        let gcs = config::ResolvedGcs {
            credentials: None,
            extension: Some("/opt/datamk/gcs.duckdb_extension".to_string()),
            key_id: None,
            secret: None,
            endpoint: None,
            use_ssl: None,
        };
        // Native mode attaches a *downloaded* catalog copy (the extension
        // cannot ATTACH a remote database file), overriding its recorded
        // DATA_PATH — same shape as engine::open_artifact.
        let sql = attach_sql(
            "orders",
            "/cell/.cell/attach/00000003.ducklake",
            "gs://bkt/cells/orders/data",
            None,
            Some(&gcs),
            true,
        )
        .unwrap();
        // httpfs first: DuckLake autoloads it for the remote DATA_PATH, and
        // the last-registered gs:// filesystem wins — the native extension
        // must load after httpfs or httpfs shadows it and reads 403.
        assert_eq!(
            sql,
            "INSTALL httpfs;\n\
             LOAD httpfs;\n\
             LOAD '/opt/datamk/gcs.duckdb_extension';\n\
             CREATE OR REPLACE SECRET datamk_orders_gcs (TYPE GCP);\n\
             ATTACH 'ducklake:/cell/.cell/attach/00000003.ducklake' \
             AS orders (DATA_PATH 'gs://bkt/cells/orders/data', READ_ONLY, \
             OVERRIDE_DATA_PATH true);\n"
        );
    }

    #[test]
    fn attach_sql_on_gcs_without_hmac_fails_with_the_fix() {
        let err = attach_sql(
            "orders",
            "gs://bkt/cells/orders/catalog/executions/00000003.ducklake",
            "gs://bkt/cells/orders/data",
            None,
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("gcs.key_id"), "unexpected error: {err}");
        assert!(err.contains("hmac"), "unexpected error: {err}");
    }

    // --- group_thousands ---------------------------------------------------

    #[test]
    fn group_thousands_small_numbers_are_unchanged() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(5), "5");
        assert_eq!(group_thousands(999), "999");
    }

    #[test]
    fn group_thousands_inserts_commas_every_three_digits() {
        assert_eq!(group_thousands(3_200), "3,200");
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(1_000_000), "1,000,000");
        assert_eq!(group_thousands(123_456_789), "123,456,789");
    }

    #[test]
    fn group_thousands_handles_negative_numbers() {
        assert_eq!(group_thousands(-3_200), "-3,200");
    }

    // --- format_timestamp_mark ----------------------------------------------

    #[test]
    fn format_timestamp_mark_utc_offset_becomes_z() {
        assert_eq!(
            format_timestamp_mark("2026-07-04 11:58:00+00"),
            "2026-07-04T11:58:00Z"
        );
    }

    #[test]
    fn format_timestamp_mark_non_utc_offset_keeps_offset() {
        assert_eq!(
            format_timestamp_mark("2026-07-04 06:58:00-05"),
            "2026-07-04T06:58:00-05"
        );
    }

    // --- format_status_lines -------------------------------------------------

    fn present(name: &str, cursor: &str, mark: &str, rows: i64) -> SourceWatermark {
        SourceWatermark {
            name: name.to_string(),
            cursor_column: cursor.to_string(),
            state: WatermarkState::Present {
                mark: mark.to_string(),
                last_delta_rows: rows,
            },
        }
    }

    fn absent(name: &str, cursor: &str) -> SourceWatermark {
        SourceWatermark {
            name: name.to_string(),
            cursor_column: cursor.to_string(),
            state: WatermarkState::Absent,
        }
    }

    #[test]
    fn status_lines_match_adr_sample_exactly() {
        let rows = vec![
            present("events", "updated_at", "2026-07-04T11:58:00Z", 3_200),
            absent("signups", "id"),
        ];
        let lines = format_status_lines(&rows);
        assert_eq!(
            lines,
            vec![
                "  events    cursor=updated_at   mark=2026-07-04T11:58:00Z   (+3,200 rows last run)",
                "  signups   cursor=id           absent — next run bootstraps a full scan",
            ]
        );
    }

    #[test]
    fn status_lines_zero_delta_rows_says_no_new_rows() {
        let rows = vec![present("events", "updated_at", "2026-07-04T11:58:00Z", 0)];
        let lines = format_status_lines(&rows);
        assert_eq!(
            lines,
            vec![
                "  events   cursor=updated_at   mark=2026-07-04T11:58:00Z   (no new rows last run)"
            ]
        );
    }

    #[test]
    fn status_lines_single_source_pads_to_its_own_width() {
        let rows = vec![present("events", "id", "42", 7)];
        let lines = format_status_lines(&rows);
        assert_eq!(
            lines,
            vec!["  events   cursor=id   mark=42   (+7 rows last run)"]
        );
    }

    // --- last_run_summary_lines (run.json narration) --------------------

    fn sample_summary() -> engine::run_summary::RunSummary {
        engine::run_summary::RunSummary {
            execution: 47,
            snapshot_id: Some(12),
            started_at: "2026-07-13T10:00:00Z".to_string(),
            finished_at: "2026-07-13T10:00:05Z".to_string(),
            datamk_version: "0.0.7".to_string(),
            verify_outcome: "passed".to_string(),
            sources: vec![
                engine::run_summary::SourceRunInfo {
                    name: "raw_spend_hourly".to_string(),
                    connection: Some("dw_silver".to_string()),
                    kind: Some("query".to_string()),
                    staged_rows: Some(59_542_301),
                    bytes_scanned: Some(987_654_321),
                },
                engine::run_summary::SourceRunInfo {
                    name: "raw_flights".to_string(),
                    connection: Some("dw_silver".to_string()),
                    kind: Some("table".to_string()),
                    staged_rows: None,
                    bytes_scanned: None,
                },
                engine::run_summary::SourceRunInfo {
                    name: "raw_orders".to_string(),
                    connection: None,
                    kind: None,
                    staged_rows: None,
                    bytes_scanned: None,
                },
            ],
            transforms: vec![
                engine::run_summary::TransformRunInfo {
                    file: "sql/stg_orders.sql".to_string(),
                    duration_ms: 42,
                },
                engine::run_summary::TransformRunInfo {
                    file: "sql/orders_daily.sql".to_string(),
                    duration_ms: 8,
                },
            ],
        }
    }

    #[test]
    fn last_run_summary_lines_narrates_sources_that_staged_something_only() {
        let lines = last_run_summary_lines(&sample_summary());
        assert_eq!(
            lines,
            vec![
                "last run (execution 47): verify passed, 2 transforms in 50 ms",
                "  raw_spend_hourly: 59,542,301 rows staged (987,654,321 bytes scanned)",
            ],
            "a table source with nothing staged and a raw source must not appear"
        );
    }

    #[test]
    fn last_run_summary_lines_omits_the_bytes_parenthetical_when_absent() {
        let mut summary = sample_summary();
        summary.sources[0].bytes_scanned = None;
        let lines = last_run_summary_lines(&summary);
        assert_eq!(lines[1], "  raw_spend_hourly: 59,542,301 rows staged");
    }

    #[test]
    fn last_run_summary_lines_singular_transform_wording() {
        let mut summary = sample_summary();
        summary.transforms.truncate(1);
        summary.sources.clear();
        let lines = last_run_summary_lines(&summary);
        assert_eq!(
            lines,
            vec!["last run (execution 47): verify passed, 1 transform in 42 ms"]
        );
    }

    // --- build_rollback_changes / format_rollback_lines ----------------------

    fn row(source: &str, cursor: &str, mark_ts: Option<&str>, rows: i64) -> RawWatermarkRow {
        RawWatermarkRow {
            source: source.to_string(),
            cursor_column: cursor.to_string(),
            mark_ts: mark_ts.map(str::to_string),
            mark_date: None,
            mark_int: None,
            last_delta_rows: rows,
        }
    }

    fn int_row(source: &str, cursor: &str, mark_int: i64, rows: i64) -> RawWatermarkRow {
        RawWatermarkRow {
            source: source.to_string(),
            cursor_column: cursor.to_string(),
            mark_ts: None,
            mark_date: None,
            mark_int: Some(mark_int),
            last_delta_rows: rows,
        }
    }

    #[test]
    fn rollback_diff_rewind_when_marks_differ() {
        let declared = vec![("events".to_string(), "updated_at".to_string())];
        let current = vec![row(
            "events",
            "updated_at",
            Some("2026-07-04 11:58:00+00"),
            3_200,
        )];
        let target = vec![row(
            "events",
            "updated_at",
            Some("2026-07-04 09:58:00+00"),
            1_000,
        )];
        let changes = build_rollback_changes(&declared, &current, &target);
        assert_eq!(
            changes,
            vec![(
                "events".to_string(),
                RollbackChange::Rewind {
                    cursor: "updated_at".to_string(),
                    from: "2026-07-04T11:58:00Z".to_string(),
                    to: "2026-07-04T09:58:00Z".to_string(),
                }
            )]
        );

        let lines = format_rollback_lines(&changes, 5);
        assert_eq!(
            lines,
            vec![
                "  events   watermark rewinds updated_at 2026-07-04T11:58:00Z -> 2026-07-04T09:58:00Z;",
                "           next run re-ingests rows where updated_at > 2026-07-04T09:58:00Z",
            ]
        );
    }

    #[test]
    fn rollback_diff_absent_in_target_only() {
        let declared = vec![("events".to_string(), "updated_at".to_string())];
        let current = vec![row(
            "events",
            "updated_at",
            Some("2026-07-04 11:58:00+00"),
            3_200,
        )];
        let target: Vec<RawWatermarkRow> = vec![];
        let changes = build_rollback_changes(&declared, &current, &target);
        assert_eq!(
            changes,
            vec![("events".to_string(), RollbackChange::AbsentInTarget)]
        );

        let lines = format_rollback_lines(&changes, 5);
        assert_eq!(
            lines,
            vec![
                "  events   watermark rewinds to absent (execution 5 predates incremental \
                 loading);",
                "           next run bootstraps a full scan",
            ]
        );
    }

    #[test]
    fn rollback_diff_absent_in_both_prints_nothing() {
        let declared = vec![("signups".to_string(), "id".to_string())];
        let current: Vec<RawWatermarkRow> = vec![];
        let target: Vec<RawWatermarkRow> = vec![];
        let changes = build_rollback_changes(&declared, &current, &target);
        assert!(changes.is_empty());
    }

    #[test]
    fn rollback_diff_identical_mark_in_both_prints_nothing() {
        let declared = vec![("events".to_string(), "updated_at".to_string())];
        let current = vec![row(
            "events",
            "updated_at",
            Some("2026-07-04 11:58:00+00"),
            3_200,
        )];
        let target = vec![row(
            "events",
            "updated_at",
            Some("2026-07-04 11:58:00+00"),
            3_200,
        )];
        let changes = build_rollback_changes(&declared, &current, &target);
        assert!(changes.is_empty());
    }

    #[test]
    fn rollback_diff_present_in_current_absent_in_current_is_no_op() {
        // Forward `rollback --execution` past LATEST: not one of the ADR's
        // named cases, deliberately silent (see `build_rollback_changes` doc).
        let declared = vec![("events".to_string(), "updated_at".to_string())];
        let current: Vec<RawWatermarkRow> = vec![];
        let target = vec![row(
            "events",
            "updated_at",
            Some("2026-07-04 11:58:00+00"),
            3_200,
        )];
        let changes = build_rollback_changes(&declared, &current, &target);
        assert!(changes.is_empty());
    }

    #[test]
    fn rollback_lines_multiple_sources_pad_to_widest_name() {
        let declared = vec![
            ("events".to_string(), "updated_at".to_string()),
            ("signups".to_string(), "id".to_string()),
        ];
        let current = vec![
            row(
                "events",
                "updated_at",
                Some("2026-07-04 11:58:00+00"),
                3_200,
            ),
            int_row("signups", "id", 42, 5),
        ];
        let target = vec![
            row(
                "events",
                "updated_at",
                Some("2026-07-04 09:58:00+00"),
                1_000,
            ),
            int_row("signups", "id", 41, 0),
        ];
        let changes = build_rollback_changes(&declared, &current, &target);
        let lines = format_rollback_lines(&changes, 5);
        assert_eq!(
            lines,
            vec![
                "  events    watermark rewinds updated_at 2026-07-04T11:58:00Z -> \
                 2026-07-04T09:58:00Z;",
                "            next run re-ingests rows where updated_at > 2026-07-04T09:58:00Z",
                "  signups   watermark rewinds id 42 -> 41;",
                "            next run re-ingests rows where id > 41",
            ]
        );
    }
}
