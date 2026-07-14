//! All BigQuery-specific connector code: extension load, ATTACH, table-path
//! validation/quoting, ADC, and (view-backed sources) classifying a table vs.
//! a view/materialized-view/external table and reading each through the
//! right API. Realized via the DuckDB community `bigquery` extension, which
//! reads through the BigQuery **Storage Read API** — an API that cannot read
//! logical/materialized views or external tables ("non-table entities cannot
//! be read with the storage API"). Those instead route through
//! `bigquery_query()`, the extension's table function over the BigQuery
//! **jobs API** (arbitrary GoogleSQL, billed as a query job).

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;

use super::{quote, CursorPredicate, ObjectKind, ObjectMeta};
use crate::engine::{esc, MarkValue};

pub(super) const INSTALL_LOAD_SQL: &str = "INSTALL bigquery FROM community; LOAD bigquery;";

/// The ATTACH statement. `IF NOT EXISTS` + an alias keyed on the connection
/// name means a connection shared by several sources attaches once.
pub(super) fn attach_sql(project: &str, billing_project: Option<&str>, alias: &str) -> String {
    let mut cs = format!("project={project}");
    if let Some(bp) = billing_project {
        cs.push_str(&format!(" billing_project={bp}"));
    }
    format!(
        "ATTACH IF NOT EXISTS '{}' AS \"{}\" (TYPE bigquery, READ_ONLY);",
        esc(&cs),
        quote(alias)
    )
}

/// Validate + quote a `dataset.table` path, resolving it under the attach
/// alias for a DuckDB (Storage Read API) reference.
pub(super) fn qualify(alias: &str, table: &str) -> Result<String> {
    let (dataset, tbl) = split_dataset_table(table)?;
    Ok(format!(
        "\"{}\".\"{}\".\"{}\"",
        quote(alias),
        quote(dataset),
        quote(tbl)
    ))
}

/// `dataset.table` — exactly two non-empty dot-separated parts; the project
/// comes from the connection, never a third part.
fn split_dataset_table(table: &str) -> Result<(&str, &str)> {
    match table.split('.').collect::<Vec<_>>().as_slice() {
        [dataset, tbl] if !dataset.is_empty() && !tbl.is_empty() => Ok((dataset, tbl)),
        _ => bail!(
            "bigquery source table must be `dataset.table`, got '{table}' \
             (the project comes from the connection; a cross-project read \
             is a second connection, not a three-part name)"
        ),
    }
}

/// Point Application Default Credentials at `path` for the rest of this
/// process. The env var name lives only here — the rest of the engine just
/// knows "the connector's credential mechanism", not that it's ADC.
pub(super) fn point_adc_at(path: &str) {
    std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", path);
}

/// A BigQuery identifier for GoogleSQL backtick-quoting. A backtick in the
/// identifier is rejected rather than escaped — `qualify()`/`split_dataset_table`
/// already constrain table-path shape, so a backtick here can only be an
/// operator error (or something adversarial) worth surfacing loudly, not
/// silently working around.
fn bq_ident(s: &str) -> Result<&str> {
    if s.contains('`') {
        bail!("identifier '{s}' contains a backtick, which is not supported here");
    }
    Ok(s)
}

/// Classify every table in `tables` (all belonging to one connection) as
/// `ObjectKind::Table` or `::Query`, batched into one metadata job per
/// dataset present in the list (BigQuery's `INFORMATION_SCHEMA.TABLES`/
/// `COLUMNS` are dataset-scoped, so that is the natural batching boundary).
pub(super) fn classify_objects(
    conn: &duckdb::Connection,
    project: &str,
    billing_project: Option<&str>,
    tables: &[&str],
) -> Result<IndexMap<String, ObjectMeta>> {
    let mut by_dataset: IndexMap<&str, Vec<(&str, &str)>> = IndexMap::new();
    for &t in tables {
        let (dataset, tbl) = split_dataset_table(t)?;
        by_dataset.entry(dataset).or_default().push((t, tbl));
    }

    let mut out = IndexMap::new();
    for (dataset, group) in by_dataset {
        classify_one_dataset(conn, project, billing_project, dataset, &group, &mut out)?;
    }
    Ok(out)
}

fn classify_one_dataset(
    conn: &duckdb::Connection,
    project: &str,
    billing_project: Option<&str>,
    dataset: &str,
    group: &[(&str, &str)],
    out: &mut IndexMap<String, ObjectMeta>,
) -> Result<()> {
    let proj_ident = bq_ident(project)?;
    let ds_ident = bq_ident(dataset)?;

    let mut in_list_parts = Vec::with_capacity(group.len());
    for (_, tbl) in group {
        in_list_parts.push(format!("'{}'", esc(bq_ident(tbl)?)));
    }
    let in_list = in_list_parts.join(", ");

    // One UNION ALL job carries both TABLES (kind) and COLUMNS (BQ-native
    // type, needed later for cursor-literal rendering) with a discriminator
    // column, rather than two separate jobs.
    let google = format!(
        "SELECT 'T' AS k, table_name AS n, table_type AS v1, CAST(NULL AS STRING) AS v2 \
         FROM `{proj_ident}.{ds_ident}`.INFORMATION_SCHEMA.TABLES WHERE table_name IN ({in_list}) \
         UNION ALL \
         SELECT 'C', table_name, column_name, data_type \
         FROM `{proj_ident}.{ds_ident}`.INFORMATION_SCHEMA.COLUMNS WHERE table_name IN ({in_list})"
    );
    let billing = billing_project.unwrap_or(project);
    let sql = format!(
        "SELECT k, n, v1, v2 FROM bigquery_query('{}', '{}', billing_project := '{}')",
        esc(project),
        esc(&google),
        esc(billing)
    );

    let mut stmt = conn
        .prepare(&sql)
        .context("preparing BigQuery metadata classification job")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .context("running BigQuery metadata classification job")?;

    let mut table_types: IndexMap<String, String> = IndexMap::new();
    let mut columns: IndexMap<String, IndexMap<String, String>> = IndexMap::new();
    for row in rows {
        let (k, n, v1, v2) = row.context("reading BigQuery metadata classification row")?;
        match k.as_str() {
            "T" => {
                if let Some(table_type) = v1 {
                    table_types.insert(n, table_type);
                }
            }
            "C" => {
                if let (Some(col), Some(ty)) = (v1, v2) {
                    columns.entry(n).or_default().insert(col, ty);
                }
            }
            _ => {}
        }
    }

    for (whole, tbl) in group {
        let Some(table_type) = table_types.get(*tbl) else {
            bail!(
                "BigQuery table '{dataset}.{tbl}' was not found by \
                 `{proj_ident}.{ds_ident}`.INFORMATION_SCHEMA.TABLES — check the table exists \
                 and the connection's credentials can see it"
            );
        };
        // Named here (not just inside `classify_table_type`'s own message):
        // classification batches every sibling table in the dataset, so the
        // table that actually fails may not be the one whose bind triggered
        // the batch — the error must self-identify regardless of which
        // source's `with_context` ends up wrapping it.
        let kind =
            classify_table_type(table_type).with_context(|| format!("table '{dataset}.{tbl}'"))?;
        let cols = columns.get(*tbl).cloned().unwrap_or_default();
        out.insert(
            (*whole).to_string(),
            ObjectMeta {
                kind,
                columns: cols,
            },
        );
    }
    Ok(())
}

/// Closed-set classification of BigQuery's `INFORMATION_SCHEMA.TABLES.table_type`.
/// An unrecognized value is a hard bind-time error naming the value and the
/// supported set — never a silent warn-and-route.
fn classify_table_type(table_type: &str) -> Result<ObjectKind> {
    match table_type {
        "BASE TABLE" | "CLONE" | "SNAPSHOT" => Ok(ObjectKind::Table),
        "VIEW" | "MATERIALIZED VIEW" | "EXTERNAL" => Ok(ObjectKind::Query),
        other => bail!(
            "BigQuery table_type '{other}' is not a value datamk recognizes; supported: \
             BASE TABLE, CLONE, SNAPSHOT (read via the Storage API), VIEW, MATERIALIZED VIEW, \
             EXTERNAL (read via the jobs API)"
        ),
    }
}

/// Best-effort detection of "the metadata job itself was denied" (missing
/// `bigquery.jobs.create`) vs. a genuine failure that must propagate. Matched
/// on error text — `bigquery_query()` surfaces the upstream BigQuery API
/// failure as a plain string, not a typed error datamk can match on.
///
/// Deliberately narrow: BigQuery's own "response too large" failure (see
/// `rewrite_response_too_large`) is wrapped by the extension in a
/// `Permission Error: BigQuery Permission Denied` preamble that has nothing
/// to do with IAM, so a loose "contains 'permission denied'" match would
/// misclassify it as jobs-create denial and make `ClassifyCache` silently
/// assume BASE TABLE — resurfacing the confusing Storage API error instead
/// of the actionable size-limit one. Match only the two shapes that actually
/// mean "the job itself was denied", and explicitly exclude the
/// response-too-large text even if a future wrapper adds "denied" wording
/// near it.
pub(super) fn is_jobs_permission_denied(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    if msg.contains("Response too large") {
        return false;
    }
    let lower = msg.to_lowercase();
    lower.contains("bigquery.jobs.create") || lower.contains("access denied")
}

/// A storage-read failure whose text names BigQuery's Storage Read API
/// rejecting a non-table entity, rewritten into the actionable message;
/// anything else is wrapped with plain context and returned unchanged in
/// substance.
pub(super) fn rewrite_view_leak(err: duckdb::Error, name: &str, table: &str) -> anyhow::Error {
    let msg = err.to_string();
    if msg.contains("non-table entities") {
        anyhow::anyhow!(
            "source '{name}' ({table}) is a BigQuery view or other non-table entity; the \
             Storage Read API cannot read it. datamk routes these through the jobs API \
             automatically, but that needs `bigquery.jobs.create` on the connection's billing \
             project, which appears to be missing. Grant it, or point `table:` at a base table."
        )
    } else {
        anyhow::Error::new(err).context(format!("probing source '{name}' ({table}) before bind"))
    }
}

/// Detects BigQuery's own "response too large" ceiling error, factored out
/// of `rewrite_response_too_large` so the engine can branch on it directly
/// (ADR 0006 §3a): on this exact signature, and only this one, a connection
/// with `staging_uri:` set escalates to `EXPORT DATA` instead of failing.
/// Anything else re-raises unchanged — the escalation trigger is a string
/// match on BigQuery's message, deliberately narrow (see the doc comment
/// below).
pub(super) fn is_response_too_large(err: &duckdb::Error) -> bool {
    err.to_string().contains("Response too large to return")
}

/// A jobs-API staging failure whose text carries BigQuery's own "response too
/// large" message, rewritten into the actionable message; anything else is
/// wrapped with `context` and returned unchanged in substance.
///
/// BigQuery's jobs API materializes a query's full result into an anonymous
/// destination table capped at ~10GB; past that it fails with "Response too
/// large to return", which the `bigquery` extension then wraps in a
/// `Permission Error: BigQuery Permission Denied` preamble — making a size
/// limit look like an IAM problem. See `is_jobs_permission_denied`, which
/// this same wrapper text would otherwise false-positive.
///
/// Called only when the connection has no `staging_uri:` (or for a
/// non-ceiling failure) — the ceiling-with-`staging_uri` case escalates to
/// `EXPORT DATA` instead (`engine::stage_via_export`) and never reaches this
/// rewrite. `describe` names what was being read: a table/view path, or
/// `"query"` for an ADR 0007 `query:` source (there is no table name there).
///
/// The fix list leads with `query:` (ADR 0007's ruling): a finance-cell
/// author must not be pointed at `incremental:` first — its bootstrap read
/// is still unbounded, so it does not, by itself, fix a ceiling hit on a
/// large source's first run.
pub(super) fn rewrite_response_too_large(
    err: duckdb::Error,
    name: &str,
    describe: &str,
    context: &str,
) -> anyhow::Error {
    if is_response_too_large(&err) {
        anyhow::anyhow!(
            "source '{name}' ({describe}): the read exceeds BigQuery's ~10GB response ceiling, \
             so it cannot be materialized in one pass. This is a size limit, not a permissions \
             problem — the IAM roles named in the underlying error are a red herring. Reduce \
             what the read returns: use a `query:` source to aggregate or project \
             server-side; `incremental:` with a cursor can help steady-state but does not \
             bound the bootstrap read; or set `staging_uri:` on the connection to stage \
             oversized results through object storage."
        )
    } else {
        anyhow::Error::new(err).context(context.to_string())
    }
}

/// The SELECT the engine wraps in `CREATE TEMP TABLE … AS`, routed by
/// `meta.kind`.
pub(super) fn read_sql(
    alias: &str,
    project: &str,
    billing_project: Option<&str>,
    table: &str,
    meta: &ObjectMeta,
    predicate: Option<&CursorPredicate>,
) -> Result<String> {
    match meta.kind {
        ObjectKind::Table => storage_read_sql(alias, table, predicate),
        ObjectKind::Query => jobs_read_sql(project, billing_project, table, meta, predicate),
    }
}

/// Storage Read API path (BASE TABLE/CLONE/SNAPSHOT) — byte-identical to the
/// pre-view-routing SQL (`bind_source`'s plain arm and `stage_incremental`'s
/// staging SELECT), so pushdown and every existing test are preserved.
fn storage_read_sql(
    alias: &str,
    table: &str,
    predicate: Option<&CursorPredicate>,
) -> Result<String> {
    let qualified = qualify(alias, table)?;
    Ok(match predicate {
        Some(p) => {
            let cq = p.cursor.replace('"', "\"\"");
            format!(
                "SELECT * FROM {qualified} WHERE \"{cq}\" > {}",
                p.mark.as_literal()
            )
        }
        None => format!("SELECT * FROM {qualified}"),
    })
}

/// The bare GoogleSQL `SELECT` shared by `jobs_read_sql` and `export_data_stmt`
/// (ADR 0006 §3a): fully-qualified backticked table, watermark predicate (if
/// any) baked in. Kept as its own function so the plain jobs read and the
/// oversized-result `EXPORT DATA` escape hatch can never drift apart — §3a
/// requires exporting "exactly the same GoogleSQL SELECT" this builds.
fn google_select(
    project: &str,
    table: &str,
    meta: &ObjectMeta,
    predicate: Option<&CursorPredicate>,
) -> Result<String> {
    let (dataset, tbl) = split_dataset_table(table)?;
    let proj_ident = bq_ident(project)?;
    let ds_ident = bq_ident(dataset)?;
    let tbl_ident = bq_ident(tbl)?;

    let mut google = format!("SELECT * FROM `{proj_ident}.{ds_ident}.{tbl_ident}`");
    if let Some(p) = predicate {
        let bq_type = meta
            .columns
            .iter()
            .find(|(c, _)| c.eq_ignore_ascii_case(p.cursor))
            .map(|(_, t)| t.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "incremental cursor '{}' was not found among {table}'s BigQuery columns \
                     during classification (internal error) — please report this",
                    p.cursor
                )
            })?;
        let lit = render_bq_predicate(p.cursor, p.mark, bq_type)?;
        let col = bq_ident(p.cursor)?;
        google.push_str(&format!(" WHERE `{col}` > {lit}"));
    }
    Ok(google)
}

/// Wrap `body` — a bare GoogleSQL `SELECT`, either engine-generated
/// (`google_select`, ADR 0006 tables/views) or author-owned verbatim
/// (`query:` sources, ADR 0007 — no identifier rewriting, no predicate
/// injection, `esc()` only for delivery) — in `bigquery_query(...)`, the
/// extension's jobs-API read. `dry_run` appends `, dry_run := true` (ADR
/// 0007 §4's preflight) — `false` reproduces the real-read call
/// byte-for-byte, unchanged from before this parameter existed.
fn wrap_bigquery_query(
    project: &str,
    billing_project: Option<&str>,
    body: &str,
    dry_run: bool,
) -> String {
    let billing = billing_project.unwrap_or(project);
    let dry_run_arg = if dry_run { ", dry_run := true" } else { "" };
    format!(
        "SELECT * FROM bigquery_query('{}', '{}', billing_project := '{}'{dry_run_arg})",
        esc(project),
        esc(body),
        esc(billing)
    )
}

/// Jobs API path (VIEW/MATERIALIZED VIEW/EXTERNAL): `bigquery_query()` over a
/// fully-qualified GoogleSQL SELECT, with the watermark predicate (if any)
/// baked into the issued query rather than applied by DuckDB afterwards.
fn jobs_read_sql(
    project: &str,
    billing_project: Option<&str>,
    table: &str,
    meta: &ObjectMeta,
    predicate: Option<&CursorPredicate>,
) -> Result<String> {
    let google = google_select(project, table, meta, predicate)?;
    Ok(wrap_bigquery_query(
        project,
        billing_project,
        &google,
        false,
    ))
}

/// ADR 0007 §2: the jobs-API read for an author-owned `query:` source. The
/// connector's only transformation of `query` is `esc()` for delivery —
/// no identifier rewriting, no predicate injection (`incremental:` is
/// refused on this shape at resolve time, so there is never a predicate to
/// inject). Sibling to `jobs_read_sql`, minus `google_select`: there is no
/// table to qualify and nothing generated — `query` is the whole read.
pub(super) fn query_read_sql(project: &str, billing_project: Option<&str>, query: &str) -> String {
    wrap_bigquery_query(project, billing_project, query, false)
}

/// ADR 0007 §4: the dry-run preflight for a `query:` source — the same
/// `bigquery_query()` call `query_read_sql` issues, `dry_run := true`
/// appended. Free (no scan billed) and returns exactly one row of
/// `(total_bytes_processed BIGINT, cache_hit BOOLEAN, location VARCHAR)`,
/// live-verified — the engine reads those three columns positionally.
pub(super) fn query_dry_run_sql(
    project: &str,
    billing_project: Option<&str>,
    query: &str,
) -> String {
    wrap_bigquery_query(project, billing_project, query, true)
}

/// Best-effort classification of a `query:` dry-run preflight failure (ADR
/// 0007 §4): a deliberately narrow set of BigQuery error shapes that are
/// unambiguously "the author's query itself is wrong" — a syntax error, an
/// unresolvable identifier, a missing table/dataset. Anything else
/// (an unrecognized shape, an ambiguously-wrapped error, a genuine
/// transport hiccup) is **not** classified as a query error, so the
/// preflight only warns and the engine proceeds to the real staging read,
/// which fails loud on its own if the query really is broken. The
/// asymmetry is deliberate: misclassifying toward "gate loud" would turn a
/// transient dry-run blip into a bind failure for an otherwise-good query;
/// misclassifying toward "warn" costs one avoidable real read at worst.
pub(super) fn is_dry_run_query_error(err: &duckdb::Error) -> bool {
    let msg = err.to_string();
    [
        "Syntax error",
        "Invalid query",
        "Unrecognized name",
        "Not found: Table",
        "Not found: Dataset",
        "Duplicate column",
    ]
    .iter()
    .any(|needle| msg.contains(needle))
}

/// Wrap `body` — a bare GoogleSQL `SELECT` — in the `EXPORT DATA` statement
/// ADR 0006 §3a's escape hatch runs: writes `body`'s result as parquet to
/// `run_prefix` instead of BigQuery's anonymous result table. `run_prefix`
/// is the run-scoped scratch location (`<staging_uri>/<cell>/<run>`, no
/// trailing slash required); the `*` wildcard file pattern is this
/// function's concern, not the caller's — a `uri` with no wildcard is
/// rejected by BigQuery.
fn wrap_export_data(body: &str, run_prefix: &str) -> String {
    let prefix = run_prefix.trim_end_matches('/');
    format!(
        "EXPORT DATA OPTIONS(uri='{}/part-*.parquet', format='PARQUET') AS {body}",
        esc(prefix)
    )
}

/// The bare `EXPORT DATA` statement (no DuckDB wrapper) for ADR 0006 §3a's
/// oversized-result escape hatch: the same GoogleSQL `SELECT`
/// `jobs_read_sql` would issue (via `google_select` — predicate included,
/// reused verbatim), writing parquet to `run_prefix` instead of BigQuery's
/// anonymous result table.
fn export_data_stmt(
    project: &str,
    table: &str,
    meta: &ObjectMeta,
    predicate: Option<&CursorPredicate>,
    run_prefix: &str,
) -> Result<String> {
    let google = google_select(project, table, meta, predicate)?;
    Ok(wrap_export_data(&google, run_prefix))
}

/// Wrap an already-built GoogleSQL statement in a bare-project
/// `CALL bigquery_execute(...)` — the form verified live (ADR 0006 §3a) to
/// run and bill the job in `billing_or_project` even though the
/// connection's attach alias stays `READ_ONLY` (`bigquery_execute` issued
/// through the alias fails: "Cannot execute BigQuery query in read-only
/// transaction"). `esc()` doubles single quotes in `stmt` exactly once —
/// the same one-level nesting discipline `jobs_read_sql` uses for its
/// embedded `google` text — so every literal `stmt` itself carries
/// (`uri='…'`, a predicate literal) survives being embedded inside this
/// call's own single-quoted argument.
fn call_bigquery_execute(billing_or_project: &str, stmt: &str) -> String {
    format!(
        "CALL bigquery_execute('{}', '{}')",
        esc(billing_or_project),
        esc(stmt)
    )
}

/// Pure: the full statement `engine::stage_via_export` executes for §3a's
/// escape hatch — build the `EXPORT DATA` statement, then wrap it for the
/// bare-project `bigquery_execute` call. `billing_project` defaults to
/// `project`, same default `jobs_read_sql` uses for billing.
pub(super) fn export_sql(
    project: &str,
    billing_project: Option<&str>,
    table: &str,
    meta: &ObjectMeta,
    predicate: Option<&CursorPredicate>,
    run_prefix: &str,
) -> Result<String> {
    let stmt = export_data_stmt(project, table, meta, predicate, run_prefix)?;
    let billing = billing_project.unwrap_or(project);
    Ok(call_bigquery_execute(billing, &stmt))
}

/// ADR 0007 §2: the `EXPORT DATA` statement for an oversized `query:`
/// source's escalation — the export wraps the author's query verbatim
/// (`esc()` only) instead of a generated `SELECT *`. Sibling to
/// `export_sql`, minus `google_select`/`export_data_stmt`: `query` is
/// already the whole body to export.
pub(super) fn query_export_sql(
    project: &str,
    billing_project: Option<&str>,
    query: &str,
    run_prefix: &str,
) -> String {
    let stmt = wrap_export_data(query, run_prefix);
    let billing = billing_project.unwrap_or(project);
    call_bigquery_execute(billing, &stmt)
}

/// Render `mark` as a GoogleSQL literal for the BigQuery-native `data_type`
/// of the cursor column. Keys off the BQ-native type (never DuckDB's) because
/// DuckDB cannot distinguish BigQuery TIMESTAMP from DATETIME — both surface
/// as its own `timestamp` — and comparing a DATETIME column to a TIMESTAMP
/// literal is a hard type error in BigQuery.
fn render_bq_predicate(cursor: &str, mark: &MarkValue, bq_type: &str) -> Result<String> {
    let ty = bq_type.to_uppercase();
    match (ty.as_str(), mark) {
        ("TIMESTAMP", MarkValue::Ts(s)) => Ok(format!("TIMESTAMP '{}'", esc(&with_utc_offset(s)))),
        ("DATETIME", MarkValue::Ts(s)) => Ok(format!("DATETIME '{}'", esc(&strip_offset(s)))),
        ("DATE", MarkValue::Date(s)) => Ok(format!("DATE '{}'", esc(s))),
        ("INT64", MarkValue::Int(n)) => Ok(n.to_string()),
        _ => bail!(
            "incremental cursor '{cursor}' has BigQuery type {bq_type}, which a view-routed \
             predicate does not support; supported types: TIMESTAMP, DATETIME, DATE, INT64"
        ),
    }
}

/// A `MarkValue::Ts` string is naive-UTC text that may or may not already
/// carry a UTC offset suffix depending on where it was rendered
/// (`__datamk_watermarks.mark_ts` is TIMESTAMPTZ and always carries one via
/// `::VARCHAR`; a naive TIMESTAMP source read would not). BigQuery TIMESTAMP
/// literals need the offset explicit. Only text after the date's own hyphens
/// (index > 10, past `YYYY-MM-DD`) is treated as an offset sign.
fn with_utc_offset(s: &str) -> String {
    if has_offset(s) {
        s.to_string()
    } else {
        format!("{s}+00")
    }
}

/// The inverse of `with_utc_offset`: BigQuery DATETIME is offset-less, so a
/// carried `+00`/`Z` must be stripped before rendering the literal.
fn strip_offset(s: &str) -> String {
    if let Some(stripped) = s.strip_suffix('Z') {
        return stripped.to_string();
    }
    if let Some(pos) = s.rfind(['+', '-']).filter(|&i| i > 10) {
        return s[..pos].to_string();
    }
    s.to_string()
}

fn has_offset(s: &str) -> bool {
    s.trim_end().ends_with('Z') || s.rfind(['+', '-']).is_some_and(|i| i > 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> MarkValue {
        MarkValue::Ts(s.to_string())
    }

    fn meta_table() -> ObjectMeta {
        ObjectMeta {
            kind: ObjectKind::Table,
            columns: IndexMap::new(),
        }
    }

    fn meta_query(columns: &[(&str, &str)]) -> ObjectMeta {
        ObjectMeta {
            kind: ObjectKind::Query,
            columns: columns
                .iter()
                .map(|(c, t)| (c.to_string(), t.to_string()))
                .collect(),
        }
    }

    #[test]
    fn bigquery_attach_sql_is_read_only_and_aliased() {
        assert_eq!(
            attach_sql("acme-prod", None, "__conn_crm"),
            "ATTACH IF NOT EXISTS 'project=acme-prod' AS \"__conn_crm\" \
             (TYPE bigquery, READ_ONLY);"
        );
    }

    #[test]
    fn bigquery_attach_sql_includes_billing_project_when_set() {
        assert_eq!(
            attach_sql("acme-prod", Some("acme-billing"), "__conn_crm"),
            "ATTACH IF NOT EXISTS 'project=acme-prod billing_project=acme-billing' \
             AS \"__conn_crm\" (TYPE bigquery, READ_ONLY);"
        );
    }

    #[test]
    fn bigquery_qualify_accepts_dataset_table() {
        assert_eq!(
            qualify("__conn_crm", "sales.accounts").unwrap(),
            "\"__conn_crm\".\"sales\".\"accounts\""
        );
    }

    #[test]
    fn bigquery_qualify_quotes_identifiers() {
        assert_eq!(
            qualify("__conn_crm", "sa\"les.acc\"ounts").unwrap(),
            "\"__conn_crm\".\"sa\"\"les\".\"acc\"\"ounts\""
        );
    }

    #[test]
    fn bigquery_qualify_rejects_one_and_three_part_names() {
        for bad in ["accounts", "proj.sales.accounts", "sales.", ".accounts", ""] {
            let err = qualify("__conn_crm", bad).unwrap_err().to_string();
            assert!(err.contains("dataset.table"), "for '{bad}': {err}");
        }
    }

    // --- read_sql: storage path (Part 2) -----------------------------------
    // Must stay byte-identical to the pre-view-routing SQL.

    #[test]
    fn read_sql_storage_plain_matches_todays_sql() {
        let meta = meta_table();
        assert_eq!(
            read_sql("__conn_crm", "p", None, "sales.accounts", &meta, None).unwrap(),
            "SELECT * FROM \"__conn_crm\".\"sales\".\"accounts\""
        );
    }

    #[test]
    fn read_sql_storage_with_predicate_matches_todays_sql() {
        let meta = meta_table();
        let mark = MarkValue::Ts("2026-07-04 10:58:00+00".to_string());
        let pred = CursorPredicate {
            cursor: "updated_at",
            mark: &mark,
        };
        assert_eq!(
            read_sql(
                "__conn_crm",
                "p",
                None,
                "sales.accounts",
                &meta,
                Some(&pred)
            )
            .unwrap(),
            "SELECT * FROM \"__conn_crm\".\"sales\".\"accounts\" WHERE \"updated_at\" > \
             TIMESTAMPTZ '2026-07-04 10:58:00+00'"
        );
    }

    // --- read_sql: jobs path (Part 2, the new feature) ---------------------

    #[test]
    fn read_sql_jobs_plain() {
        let meta = meta_query(&[]);
        assert_eq!(
            read_sql("__conn_crm", "p", None, "sales.accounts", &meta, None).unwrap(),
            "SELECT * FROM bigquery_query('p', 'SELECT * FROM `p.sales.accounts`', \
             billing_project := 'p')"
        );
    }

    #[test]
    fn read_sql_jobs_defaults_billing_project_to_project() {
        let meta = meta_query(&[]);
        let sql = read_sql(
            "__conn_crm",
            "acme-prod",
            None,
            "sales.accounts",
            &meta,
            None,
        )
        .unwrap();
        assert!(sql.contains("billing_project := 'acme-prod'"), "{sql}");
    }

    #[test]
    fn read_sql_jobs_uses_explicit_billing_project() {
        let meta = meta_query(&[]);
        let sql = read_sql(
            "__conn_crm",
            "acme-prod",
            Some("acme-billing"),
            "sales.accounts",
            &meta,
            None,
        )
        .unwrap();
        assert!(sql.contains("billing_project := 'acme-billing'"), "{sql}");
    }

    #[test]
    fn read_sql_jobs_with_timestamp_predicate_appends_utc_offset() {
        let meta = meta_query(&[("updated_at", "TIMESTAMP")]);
        let mark = ts("2026-07-04 10:58:00");
        let pred = CursorPredicate {
            cursor: "updated_at",
            mark: &mark,
        };
        let sql = read_sql(
            "__conn_crm",
            "p",
            None,
            "sales.accounts",
            &meta,
            Some(&pred),
        )
        .unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM bigquery_query('p', 'SELECT * FROM `p.sales.accounts` WHERE \
             `updated_at` > TIMESTAMP ''2026-07-04 10:58:00+00''', billing_project := 'p')"
        );
    }

    #[test]
    fn read_sql_jobs_with_datetime_predicate_strips_offset() {
        let meta = meta_query(&[("updated_at", "DATETIME")]);
        let mark = ts("2026-07-04 10:58:00+00");
        let pred = CursorPredicate {
            cursor: "updated_at",
            mark: &mark,
        };
        let sql = read_sql(
            "__conn_crm",
            "p",
            None,
            "sales.accounts",
            &meta,
            Some(&pred),
        )
        .unwrap();
        assert!(
            sql.contains("DATETIME ''2026-07-04 10:58:00'''"),
            "expected offset stripped: {sql}"
        );
    }

    #[test]
    fn read_sql_jobs_with_date_predicate() {
        let meta = meta_query(&[("d", "DATE")]);
        let mark = MarkValue::Date("2026-07-04".to_string());
        let pred = CursorPredicate {
            cursor: "d",
            mark: &mark,
        };
        let sql = read_sql(
            "__conn_crm",
            "p",
            None,
            "sales.accounts",
            &meta,
            Some(&pred),
        )
        .unwrap();
        assert!(sql.contains("DATE ''2026-07-04'''"), "{sql}");
    }

    #[test]
    fn read_sql_jobs_with_int_predicate() {
        let meta = meta_query(&[("id", "INT64")]);
        let mark = MarkValue::Int(42);
        let pred = CursorPredicate {
            cursor: "id",
            mark: &mark,
        };
        let sql = read_sql(
            "__conn_crm",
            "p",
            None,
            "sales.accounts",
            &meta,
            Some(&pred),
        )
        .unwrap();
        assert!(sql.contains("`id` > 42"), "{sql}");
    }

    #[test]
    fn read_sql_jobs_rejects_unsupported_cursor_type() {
        let meta = meta_query(&[("id", "FLOAT64")]);
        let mark = MarkValue::Int(42);
        let pred = CursorPredicate {
            cursor: "id",
            mark: &mark,
        };
        let err = read_sql(
            "__conn_crm",
            "p",
            None,
            "sales.accounts",
            &meta,
            Some(&pred),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("FLOAT64"), "{err}");
        assert!(err.contains("TIMESTAMP, DATETIME, DATE, INT64"), "{err}");
    }

    // --- export_sql: the §3a oversized-jobs-result escape hatch ------------

    #[test]
    fn export_data_stmt_wildcard_uri_and_no_predicate() {
        let meta = meta_query(&[]);
        let sql = export_data_stmt(
            "p",
            "sales.accounts",
            &meta,
            None,
            "gs://acme-bq-staging/datamk-scratch/orders/run-42",
        )
        .unwrap();
        assert_eq!(
            sql,
            "EXPORT DATA OPTIONS(uri='gs://acme-bq-staging/datamk-scratch/orders/run-42/part-*.parquet', \
             format='PARQUET') AS SELECT * FROM `p.sales.accounts`"
        );
    }

    #[test]
    fn export_data_stmt_strips_a_trailing_slash_on_the_run_prefix() {
        let meta = meta_query(&[]);
        let sql = export_data_stmt(
            "p",
            "sales.accounts",
            &meta,
            None,
            "gs://bucket/prefix/orders/run-1/",
        )
        .unwrap();
        assert!(
            sql.contains("uri='gs://bucket/prefix/orders/run-1/part-*.parquet'"),
            "{sql}"
        );
        assert!(!sql.contains("run-1//part"), "{sql}");
    }

    #[test]
    fn export_data_stmt_reuses_the_same_select_body_as_jobs_read_sql() {
        // §3a: "the same GoogleSQL SELECT §4's read_sql builds" — the
        // export statement's AS-clause must be byte-identical to
        // `google_select`'s output for the same table/meta/predicate, and
        // `jobs_read_sql` (once its embedding quotes are undone) must carry
        // that exact same text — proof the two paths can never drift apart.
        let meta = meta_query(&[("updated_at", "TIMESTAMP")]);
        let mark = ts("2026-07-04 10:58:00");
        let pred = CursorPredicate {
            cursor: "updated_at",
            mark: &mark,
        };
        let select = google_select("p", "sales.accounts", &meta, Some(&pred)).unwrap();
        assert_eq!(
            select,
            "SELECT * FROM `p.sales.accounts` WHERE `updated_at` > TIMESTAMP '2026-07-04 \
             10:58:00+00'"
        );

        let export_stmt = export_data_stmt(
            "p",
            "sales.accounts",
            &meta,
            Some(&pred),
            "gs://bucket/prefix/orders/run-1",
        )
        .unwrap();
        assert!(
            export_stmt.ends_with(&format!(" AS {select}")),
            "export statement's AS-clause must match google_select's output verbatim: {export_stmt}"
        );

        let jobs_sql = jobs_read_sql("p", None, "sales.accounts", &meta, Some(&pred)).unwrap();
        assert!(
            jobs_sql.contains(&select.replace('\'', "''")),
            "jobs_read_sql must embed the identical select body (mod one level of \
             quote-doubling): {jobs_sql}"
        );
    }

    #[test]
    fn call_bigquery_execute_doubles_embedded_single_quotes() {
        let call = call_bigquery_execute("acme-prod", "EXPORT DATA OPTIONS(uri='gs://b/x')");
        assert_eq!(
            call,
            "CALL bigquery_execute('acme-prod', 'EXPORT DATA OPTIONS(uri=''gs://b/x'')')"
        );
    }

    #[test]
    fn export_sql_uses_bare_project_as_the_billing_arg_by_default() {
        let meta = meta_query(&[]);
        let sql = export_sql(
            "acme-prod",
            None,
            "sales.accounts",
            &meta,
            None,
            "gs://bucket/prefix/orders/run-1",
        )
        .unwrap();
        assert!(
            sql.starts_with("CALL bigquery_execute('acme-prod', '"),
            "{sql}"
        );
        assert!(sql.contains("''PARQUET''"), "{sql}");
    }

    #[test]
    fn export_sql_uses_billing_project_over_project_when_set() {
        let meta = meta_query(&[]);
        let sql = export_sql(
            "acme-prod",
            Some("acme-billing"),
            "sales.accounts",
            &meta,
            None,
            "gs://bucket/prefix/orders/run-1",
        )
        .unwrap();
        assert!(
            sql.starts_with("CALL bigquery_execute('acme-billing', '"),
            "{sql}"
        );
    }

    // --- ADR 0007: `query:` connection sources ------------------------------
    // The connector's only transformation of the author's query is `esc()`
    // for delivery — no identifier rewriting, no predicate injection.

    #[test]
    fn query_read_sql_delivers_the_author_query_verbatim_modulo_escaping() {
        let sql = query_read_sql("acme-prod", None, "SELECT * FROM `x.y.z` GROUP BY 1");
        assert_eq!(
            sql,
            "SELECT * FROM bigquery_query('acme-prod', 'SELECT * FROM `x.y.z` GROUP BY 1', \
             billing_project := 'acme-prod')"
        );
    }

    #[test]
    fn query_read_sql_escapes_a_single_quote_in_the_authors_query() {
        // No identifier rewriting, no predicate injection — `esc()` only,
        // exactly once, so the author's own literal survives the outer
        // single-quoted argument.
        let sql = query_read_sql("p", None, "SELECT 'it''s fine' AS x");
        assert_eq!(
            sql,
            "SELECT * FROM bigquery_query('p', 'SELECT ''it''''s fine'' AS x', \
             billing_project := 'p')"
        );
    }

    #[test]
    fn query_read_sql_uses_billing_project_over_project_when_set() {
        let sql = query_read_sql("acme-prod", Some("acme-billing"), "SELECT 1");
        assert!(sql.contains("billing_project := 'acme-billing'"), "{sql}");
    }

    #[test]
    fn query_export_sql_wraps_the_author_query_verbatim_with_the_wildcard_uri() {
        let sql = query_export_sql(
            "acme-prod",
            None,
            "SELECT advertiser_id, SUM(spend) FROM `x.y.z` GROUP BY 1",
            "gs://acme-bq-staging/datamk-scratch/flight-spend/run-1",
        );
        assert_eq!(
            sql,
            "CALL bigquery_execute('acme-prod', 'EXPORT DATA OPTIONS(uri=\
             ''gs://acme-bq-staging/datamk-scratch/flight-spend/run-1/part-*.parquet'', \
             format=''PARQUET'') AS SELECT advertiser_id, SUM(spend) FROM `x.y.z` GROUP BY 1')"
        );
    }

    #[test]
    fn query_export_sql_uses_billing_project_as_the_bare_first_arg() {
        let sql = query_export_sql(
            "acme-prod",
            Some("acme-billing"),
            "SELECT 1",
            "gs://bucket/prefix/run-1",
        );
        assert!(
            sql.starts_with("CALL bigquery_execute('acme-billing', '"),
            "{sql}"
        );
    }

    // --- ADR 0007 §4: dry-run preflight -------------------------------------

    #[test]
    fn query_dry_run_sql_appends_dry_run_true() {
        let sql = query_dry_run_sql("acme-prod", None, "SELECT 1");
        assert_eq!(
            sql,
            "SELECT * FROM bigquery_query('acme-prod', 'SELECT 1', billing_project := \
             'acme-prod', dry_run := true)"
        );
    }

    #[test]
    fn query_dry_run_sql_uses_billing_project_over_project_when_set() {
        let sql = query_dry_run_sql("acme-prod", Some("acme-billing"), "SELECT 1");
        assert!(sql.contains("billing_project := 'acme-billing'"), "{sql}");
        assert!(sql.ends_with("dry_run := true)"), "{sql}");
    }

    #[test]
    fn query_read_sql_is_unaffected_by_the_dry_run_parameter_existing() {
        // The real-read renderer must stay byte-identical to before
        // `wrap_bigquery_query` grew a `dry_run` parameter.
        let sql = query_read_sql("p", None, "SELECT 1");
        assert_eq!(
            sql,
            "SELECT * FROM bigquery_query('p', 'SELECT 1', billing_project := 'p')"
        );
        assert!(!sql.contains("dry_run"), "{sql}");
    }

    #[test]
    fn is_dry_run_query_error_matches_clear_query_error_shapes() {
        for msg in [
            "Syntax error: Expected end of input but got keyword FROM",
            "Invalid query: missing GROUP BY",
            "Unrecognized name: totl_spend",
            "Not found: Table acme:sales.ghost",
            "Not found: Dataset acme:missing_ds",
            "Duplicate column names in the result are not supported",
        ] {
            let e = duckdb::Error::DuckDBFailure(
                duckdb::ffi::Error {
                    code: duckdb::ffi::ErrorCode::Unknown,
                    extended_code: 0,
                },
                Some(msg.to_string()),
            );
            assert!(is_dry_run_query_error(&e), "expected a query error: {msg}");
        }
    }

    #[test]
    fn is_dry_run_query_error_defaults_to_false_for_unrecognized_or_transport_shapes() {
        for msg in [
            "Connection reset by peer",
            "context deadline exceeded",
            "Service Unavailable",
            "Permission Error: BigQuery Permission Denied",
            "some completely novel error text the extension might one day emit",
        ] {
            let e = duckdb::Error::DuckDBFailure(
                duckdb::ffi::Error {
                    code: duckdb::ffi::ErrorCode::Unknown,
                    extended_code: 0,
                },
                Some(msg.to_string()),
            );
            assert!(
                !is_dry_run_query_error(&e),
                "must default to warn-and-proceed, not gate: {msg}"
            );
        }
    }

    #[test]
    fn classify_table_type_covers_the_closed_set() {
        assert_eq!(
            classify_table_type("BASE TABLE").unwrap(),
            ObjectKind::Table
        );
        assert_eq!(classify_table_type("CLONE").unwrap(), ObjectKind::Table);
        assert_eq!(classify_table_type("SNAPSHOT").unwrap(), ObjectKind::Table);
        assert_eq!(classify_table_type("VIEW").unwrap(), ObjectKind::Query);
        assert_eq!(
            classify_table_type("MATERIALIZED VIEW").unwrap(),
            ObjectKind::Query
        );
        assert_eq!(classify_table_type("EXTERNAL").unwrap(), ObjectKind::Query);
    }

    #[test]
    fn classify_table_type_rejects_an_unknown_value() {
        let err = classify_table_type("FOREIGN").unwrap_err().to_string();
        assert!(err.contains("FOREIGN"), "{err}");
        assert!(err.contains("BASE TABLE"), "{err}");
        assert!(err.contains("VIEW"), "{err}");
    }

    #[test]
    fn bq_ident_rejects_a_backtick() {
        let err = bq_ident("evil`drop").unwrap_err().to_string();
        assert!(err.contains("backtick"), "{err}");
    }

    #[test]
    fn is_jobs_permission_denied_matches_known_shapes() {
        let denied = anyhow::anyhow!("Access Denied: Job ...: Permission bigquery.jobs.create");
        assert!(is_jobs_permission_denied(&denied));
        let other = anyhow::anyhow!("Not found: Table acme:sales.ghost");
        assert!(!is_jobs_permission_denied(&other));
    }

    /// Live reality check: staging a 2.57B-row view failed with this exact
    /// wrapper text, which mentions "Permission Denied" but is actually
    /// BigQuery's ~10GB response-size ceiling. Must NOT classify as
    /// jobs-create denial — doing so makes `ClassifyCache` silently assume
    /// BASE TABLE and resurface the confusing Storage API error instead of
    /// the actionable size-limit one.
    #[test]
    fn is_jobs_permission_denied_excludes_the_response_too_large_wrapper() {
        let response_too_large = anyhow::anyhow!(
            "Permission Error: BigQuery Permission Denied\nError details from BigQuery API: \
             Permanent error, with a last message of Response too large to return. Consider \
             specifying a destination table in your job configuration."
        );
        assert!(!is_jobs_permission_denied(&response_too_large));
    }

    #[test]
    fn rewrite_view_leak_names_the_fix_for_the_storage_api_error() {
        let e = duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error {
                code: duckdb::ffi::ErrorCode::Unknown,
                extended_code: 0,
            },
            Some(
                "Binder Error: Error while creating read session: Permanent error, with a last \
                 message of request failed: non-table entities cannot be read with the storage \
                 API"
                .to_string(),
            ),
        );
        let err = rewrite_view_leak(e, "events", "analytics.events").to_string();
        assert!(
            err.contains("is a BigQuery view or other non-table entity"),
            "{err}"
        );
        assert!(err.contains("bigquery.jobs.create"), "{err}");
    }

    /// Live reality check: the exact wrapper text from staging
    /// `summarydata.campaign_group_spend_by_minute` (2.57B rows) via the jobs
    /// API. Must be rewritten past the misleading permission framing to the
    /// actual size-limit explanation.
    #[test]
    fn rewrite_response_too_large_names_the_real_limit() {
        let e = duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error {
                code: duckdb::ffi::ErrorCode::Unknown,
                extended_code: 0,
            },
            Some(
                "Permission Error: BigQuery Permission Denied\nError details from BigQuery API: \
                 Permanent error, with a last message of Response too large to return. Consider \
                 specifying a destination table in your job configuration."
                    .to_string(),
            ),
        );
        let err = rewrite_response_too_large(
            e,
            "campaign_spend",
            "summarydata.campaign_group_spend_by_minute",
            "staging view source 'campaign_spend' -> summarydata.campaign_group_spend_by_minute \
             via the jobs API",
        )
        .to_string();
        assert!(err.contains("~10GB response ceiling"), "{err}");
        assert!(err.contains("not a permissions problem"), "{err}");
        assert!(err.contains("`incremental:`"), "{err}");
        assert!(
            err.contains("`staging_uri:`"),
            "the no-`staging_uri:`-configured fix must name the field: {err}"
        );
        assert!(!err.contains("Permission Denied"), "{err}");
        // ADR 0007's ruling: lead with `query:`, not `incremental:` — a
        // finance-cell author must not be pointed at incremental first,
        // since its bootstrap read is still unbounded.
        assert!(err.contains("`query:`"), "{err}");
        let query_pos = err.find("`query:`").expect("has `query:`");
        let incremental_pos = err.find("`incremental:`").expect("has `incremental:`");
        assert!(
            query_pos < incremental_pos,
            "`query:` must be named before `incremental:` in the fix list: {err}"
        );
    }

    /// The ceiling rewrite is also reached for an ADR 0007 `query:` source
    /// itself hitting the ceiling — `describe` is `"query"`, not a table
    /// path, and the message must still read sensibly.
    #[test]
    fn rewrite_response_too_large_describes_a_query_source() {
        let e = duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error {
                code: duckdb::ffi::ErrorCode::Unknown,
                extended_code: 0,
            },
            Some(
                "Permanent error, with a last message of Response too large to return.".to_string(),
            ),
        );
        let err = rewrite_response_too_large(
            e,
            "raw_spend_hourly",
            "query",
            "staging query source 'raw_spend_hourly' via the jobs API",
        )
        .to_string();
        assert!(err.contains("source 'raw_spend_hourly' (query):"), "{err}");
        assert!(err.contains("~10GB response ceiling"), "{err}");
    }

    #[test]
    fn is_response_too_large_matches_only_the_ceiling_signature() {
        let ceiling = duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error {
                code: duckdb::ffi::ErrorCode::Unknown,
                extended_code: 0,
            },
            Some("Permanent error, with a last message of Response too large to return.".into()),
        );
        assert!(is_response_too_large(&ceiling));
        let other = duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error {
                code: duckdb::ffi::ErrorCode::Unknown,
                extended_code: 0,
            },
            Some("Not found: Table acme:sales.ghost".into()),
        );
        assert!(!is_response_too_large(&other));
    }

    #[test]
    fn rewrite_response_too_large_leaves_other_errors_wrapped_with_context() {
        let e = duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error {
                code: duckdb::ffi::ErrorCode::Unknown,
                extended_code: 0,
            },
            Some("Not found: Table acme:sales.ghost".to_string()),
        );
        let err = rewrite_response_too_large(e, "accounts", "sales.ghost", "staging context");
        assert!(err.to_string().contains("staging context"), "{err}");
        assert!(format!("{:#}", err).contains("Not found"), "{err:#}");
    }

    #[test]
    fn with_utc_offset_appends_when_absent_and_leaves_present_alone() {
        assert_eq!(
            with_utc_offset("2026-07-04 10:58:00"),
            "2026-07-04 10:58:00+00"
        );
        assert_eq!(
            with_utc_offset("2026-07-04 10:58:00+00"),
            "2026-07-04 10:58:00+00"
        );
        assert_eq!(
            with_utc_offset("2026-07-04 10:58:00Z"),
            "2026-07-04 10:58:00Z"
        );
    }

    #[test]
    fn strip_offset_removes_a_trailing_offset_or_z() {
        assert_eq!(
            strip_offset("2026-07-04 10:58:00+00"),
            "2026-07-04 10:58:00"
        );
        assert_eq!(
            strip_offset("2026-07-04 10:58:00-05:30"),
            "2026-07-04 10:58:00"
        );
        assert_eq!(strip_offset("2026-07-04 10:58:00Z"), "2026-07-04 10:58:00");
        assert_eq!(strip_offset("2026-07-04 10:58:00"), "2026-07-04 10:58:00");
    }
}
