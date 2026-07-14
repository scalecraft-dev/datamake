//! Warehouse connectors (ADR 0003; view-backed BigQuery sources — see
//! docs/adr/0003 and the not-yet-landed view-routing ADR). A connector is a
//! handful of answers — which extension(s), the ATTACH statement, table-path
//! validation, and (this module) how to classify a warehouse object and read
//! it — realized as match arms on `ResolvedConnection`. Connectors add DuckDB
//! extensions, not Rust dependencies, so there is no cargo feature to gate;
//! adding one is a new `connections`/`connectors` module pair plus one enum
//! variant plus one match arm per method here, never a trait.
//!
//! One module per connector (`bigquery`, …), mirroring
//! `config::connections::`. This file is dispatch only.

mod bigquery;

use anyhow::{bail, Result};
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::config::{ConnectionTarget, ResolvedConnection, ResolvedSource};
use crate::engine::MarkValue;

/// Which physical read path a warehouse object needs, classified from the
/// warehouse's own metadata (never from the attached catalog's DuckDB-side
/// `information_schema`, which lies about views — see `classify_objects`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// BASE TABLE / CLONE / SNAPSHOT — the Storage Read API can read it
    /// directly through the attached catalog. Today's pushdown path,
    /// unchanged.
    Table,
    /// VIEW / MATERIALIZED VIEW / EXTERNAL — the Storage Read API cannot read
    /// it ("non-table entities cannot be read with the storage API"); route
    /// through the connector's query-job path instead.
    Query,
}

/// One object's classification plus the warehouse-native column types needed
/// to render a cursor predicate correctly (DuckDB's own column type can lose
/// information the source dialect needs — e.g. BigQuery TIMESTAMP vs DATETIME
/// both surface as DuckDB's `timestamp`).
#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub kind: ObjectKind,
    /// column name -> warehouse-native `data_type` (e.g. BigQuery's
    /// `INFORMATION_SCHEMA.COLUMNS.data_type`: `TIMESTAMP`, `INT64`, …).
    pub columns: IndexMap<String, String>,
}

/// A watermark predicate to bake into a connector's read: the cursor column
/// and its lower bound. `MarkValue` stays DuckDB-native (no dialect flag) —
/// each connector renders its own literal from `(MarkValue, warehouse-native
/// type)`, e.g. `bigquery::render_bq_predicate`.
pub struct CursorPredicate<'a> {
    pub cursor: &'a str,
    pub mark: &'a MarkValue,
}

/// Per-run cache of warehouse object classification: at most one metadata job
/// per (connection, dataset), no matter how many sources in this run
/// reference tables in it. Built from every declared connection source up
/// front so the first bind that needs classification for a dataset batches
/// every sibling table into that one job.
pub struct ClassifyCache {
    /// (connection, dataset) -> every connector-scoped table path declared
    /// anywhere in this cell for that connection+dataset.
    siblings: HashMap<(String, String), Vec<String>>,
    /// (connection, dataset) -> classification results, once fetched.
    by_group: HashMap<(String, String), IndexMap<String, ObjectMeta>>,
    /// Connections where classification failed with a permission error —
    /// warned once; every subsequent source on that connection assumes BASE
    /// TABLE without asking again.
    denied: HashSet<String>,
}

impl ClassifyCache {
    pub fn new(sources: &IndexMap<String, ResolvedSource>) -> Self {
        let mut siblings: HashMap<(String, String), Vec<String>> = HashMap::new();
        for src in sources.values() {
            // ADR 0007: a `query:` source has no table to classify — it is
            // jobs-routed by construction and never reaches `ClassifyCache`
            // at all (`bind_source` skips `classify()` for it), so it
            // contributes no sibling batching here either.
            if let ResolvedSource::Connection {
                connection,
                target: ConnectionTarget::Table(table),
                ..
            } = src
            {
                let key = (connection.clone(), dataset_of(table).to_string());
                siblings.entry(key).or_default().push(table.clone());
            }
        }
        Self {
            siblings,
            by_group: HashMap::new(),
            denied: HashSet::new(),
        }
    }

    /// Classify `table` on `connection`, batching every sibling table sharing
    /// its (connection, dataset) into one job the first time that group is
    /// needed. `Ok(None)` means classification is denied for this connection
    /// (the caller falls back to assuming BASE TABLE and must independently
    /// verify that assumption before trusting it — see
    /// `engine::probe_storage_read`).
    pub fn classify(
        &mut self,
        duckdb: &duckdb::Connection,
        connection: &str,
        connector: &ResolvedConnection,
        table: &str,
    ) -> Result<Option<ObjectMeta>> {
        if self.denied.contains(connection) {
            return Ok(None);
        }
        let key = (connection.to_string(), dataset_of(table).to_string());
        if let Some(group) = self.by_group.get(&key) {
            return Ok(Some(group.get(table).cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "table '{table}' was not part of its own classification batch (internal \
                     error) — please report this"
                )
            })?));
        }

        let group_tables = self
            .siblings
            .get(&key)
            .cloned()
            .unwrap_or_else(|| vec![table.to_string()]);
        let refs: Vec<&str> = group_tables.iter().map(String::as_str).collect();

        match connector.classify_objects(duckdb, &refs) {
            Ok(meta) => {
                let result = meta.get(table).cloned();
                self.by_group.insert(key, meta);
                Ok(result)
            }
            Err(e) => {
                if connector.is_jobs_permission_denied(&e) {
                    tracing::warn!(
                        "connection '{connection}': cannot check whether its sources are views \
                         (BigQuery jobs API denied: {e}) — assuming base tables and reading via \
                         the Storage API. A view source will then fail; grant \
                         `bigquery.jobs.create` on the connection's billing project so views \
                         route automatically."
                    );
                    self.denied.insert(connection.to_string());
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }
}

/// The dataset component of a connector-scoped table path (`dataset.table`
/// today — the only shape any connector currently validates via `qualify()`,
/// called and enforced before a table ever reaches `ClassifyCache`). Used
/// only to batch classification jobs; SQL construction never touches this.
fn dataset_of(table: &str) -> &str {
    table.split_once('.').map(|(d, _)| d).unwrap_or(table)
}

impl ResolvedConnection {
    /// The connector's `type:` name, for logs and errors.
    pub fn type_name(&self) -> &'static str {
        match self {
            ResolvedConnection::Bigquery { .. } => "bigquery",
        }
    }

    /// Extension install+load. INSTALL fetches from the registry on first run
    /// (needs network); deployed images bake the extension instead (ADR 0003 §4).
    pub fn install_load_sql(&self) -> &'static str {
        match self {
            ResolvedConnection::Bigquery { .. } => bigquery::INSTALL_LOAD_SQL,
        }
    }

    /// The ATTACH statement. `IF NOT EXISTS` + an alias keyed on the connection
    /// name means a connection shared by several sources attaches once.
    pub fn attach_sql(&self, alias: &str) -> String {
        match self {
            ResolvedConnection::Bigquery {
                project,
                billing_project,
                ..
            } => bigquery::attach_sql(project, billing_project.as_deref(), alias),
        }
    }

    /// Validate + quote the connector-scoped table path against the connector's
    /// expected shape, resolving it under the attach alias.
    pub fn qualify(&self, alias: &str, table: &str) -> Result<String> {
        match self {
            ResolvedConnection::Bigquery { .. } => bigquery::qualify(alias, table),
        }
    }

    fn credentials(&self) -> Option<&str> {
        match self {
            ResolvedConnection::Bigquery { credentials, .. } => credentials.as_deref(),
        }
    }

    /// ADR 0006 §3a's scratch object-store prefix, if the connection has one
    /// configured. `None` ⇒ an oversized jobs-path result is a hard error
    /// (`rewrite_response_too_large`) rather than an escalation.
    pub fn staging_uri(&self) -> Option<&str> {
        match self {
            ResolvedConnection::Bigquery { staging_uri, .. } => staging_uri.as_deref(),
        }
    }

    /// Point the connector's credential mechanism at the resolved key-file
    /// path (BigQuery: Application Default Credentials). Called once per run
    /// from `prepare()`.
    fn point_credentials_at(&self, path: &str) {
        match self {
            ResolvedConnection::Bigquery { .. } => bigquery::point_adc_at(path),
        }
    }

    /// Impure: one metadata job per (connection, dataset) — classify every
    /// table in `tables` (all sharing one connection) as `ObjectKind::Table`
    /// or `::Query` and capture its warehouse-native column types. Callers
    /// should go through `ClassifyCache` rather than calling this directly,
    /// so a run issues at most one job per dataset.
    pub fn classify_objects(
        &self,
        conn: &duckdb::Connection,
        tables: &[&str],
    ) -> Result<IndexMap<String, ObjectMeta>> {
        match self {
            ResolvedConnection::Bigquery {
                project,
                billing_project,
                ..
            } => bigquery::classify_objects(conn, project, billing_project.as_deref(), tables),
        }
    }

    /// Pure: the SELECT the engine wraps in `CREATE TEMP TABLE … AS`, routed
    /// by the object's classified kind. `Table` reproduces today's storage
    /// path byte-for-byte; `Query` reads through the connector's jobs API.
    pub fn read_sql(
        &self,
        alias: &str,
        table: &str,
        meta: &ObjectMeta,
        predicate: Option<&CursorPredicate>,
    ) -> Result<String> {
        match self {
            ResolvedConnection::Bigquery {
                project,
                billing_project,
                ..
            } => bigquery::read_sql(
                alias,
                project,
                billing_project.as_deref(),
                table,
                meta,
                predicate,
            ),
        }
    }

    /// Whether `err` (from `classify_objects`) is the connector's shape of
    /// "the metadata job itself was denied" (e.g. BigQuery's
    /// `bigquery.jobs.create` missing) rather than a genuine failure (bad
    /// table name, malformed query) that must propagate.
    fn is_jobs_permission_denied(&self, err: &anyhow::Error) -> bool {
        match self {
            ResolvedConnection::Bigquery { .. } => bigquery::is_jobs_permission_denied(err),
        }
    }

    /// Rewrite a storage-read failure that turns out to be a view/non-table
    /// object slipping through (the classification-denied fallback's probe,
    /// or any other leak-through) into the user-facing text naming the fix.
    /// Falls back to a plain context-wrapped error for anything else.
    pub fn rewrite_view_leak(&self, err: duckdb::Error, name: &str, table: &str) -> anyhow::Error {
        match self {
            ResolvedConnection::Bigquery { .. } => bigquery::rewrite_view_leak(err, name, table),
        }
    }

    /// Rewrite a jobs-API staging failure that turns out to be the
    /// warehouse's own result-size ceiling into the user-facing text naming
    /// the actual limit (not the misleading permission framing the API
    /// wraps it in). Falls back to a plain `context`-wrapped error for
    /// anything else. `describe` names what was being read — a table/view
    /// path, or `"query"` for an ADR 0007 `query:` source.
    pub fn rewrite_response_too_large(
        &self,
        err: duckdb::Error,
        name: &str,
        describe: &str,
        context: &str,
    ) -> anyhow::Error {
        match self {
            ResolvedConnection::Bigquery { .. } => {
                bigquery::rewrite_response_too_large(err, name, describe, context)
            }
        }
    }

    /// Whether `err` (from staging a jobs-path read) is the connector's
    /// shape of "the result exceeded the warehouse's anonymous-result
    /// ceiling" (ADR 0006 §3a) — the one, and only, trigger for escalating
    /// to `export_sql`.
    pub fn is_response_too_large(&self, err: &duckdb::Error) -> bool {
        match self {
            ResolvedConnection::Bigquery { .. } => bigquery::is_response_too_large(err),
        }
    }

    /// Pure: the statement `engine::stage_via_export` executes for the §3a
    /// escape hatch — `EXPORT DATA` writing the same query `read_sql` would
    /// have issued to parquet at `run_prefix`, wrapped for the bare-project
    /// jobs-API call that runs (and bills) outside the connection's
    /// `READ_ONLY` attach.
    pub fn export_sql(
        &self,
        table: &str,
        meta: &ObjectMeta,
        predicate: Option<&CursorPredicate>,
        run_prefix: &str,
    ) -> Result<String> {
        match self {
            ResolvedConnection::Bigquery {
                project,
                billing_project,
                ..
            } => bigquery::export_sql(
                project,
                billing_project.as_deref(),
                table,
                meta,
                predicate,
                run_prefix,
            ),
        }
    }

    /// Pure: ADR 0007 §2's jobs-API read for an author-owned `query:`
    /// source — the connector's only transformation of `query` is `esc()`
    /// for delivery; no identifier rewriting, no predicate injection. Jobs-
    /// routed by construction: unlike `read_sql`, there is no `meta` to
    /// route on, because a `query:` source is never classified at all.
    pub fn query_read_sql(&self, query: &str) -> String {
        match self {
            ResolvedConnection::Bigquery {
                project,
                billing_project,
                ..
            } => bigquery::query_read_sql(project, billing_project.as_deref(), query),
        }
    }

    /// Pure: ADR 0007 §2's `EXPORT DATA` statement for an oversized
    /// `query:` source's §3a escalation — the export wraps the author's
    /// query verbatim instead of a generated `SELECT *`. Sibling to
    /// `export_sql`.
    pub fn query_export_sql(&self, query: &str, run_prefix: &str) -> String {
        match self {
            ResolvedConnection::Bigquery {
                project,
                billing_project,
                ..
            } => bigquery::query_export_sql(project, billing_project.as_deref(), query, run_prefix),
        }
    }

    /// Pure: ADR 0007 §4's dry-run preflight statement for a `query:`
    /// source — the same `bigquery_query()` call `query_read_sql` issues,
    /// `dry_run := true` appended.
    pub fn query_dry_run_sql(&self, query: &str) -> String {
        match self {
            ResolvedConnection::Bigquery {
                project,
                billing_project,
                ..
            } => bigquery::query_dry_run_sql(project, billing_project.as_deref(), query),
        }
    }

    /// Whether `err` (from the ADR 0007 §4 dry-run preflight) is the
    /// connector's shape of "the author's query itself is wrong" —
    /// narrowly matched; anything else is treated as a transport-ish
    /// failure the preflight only warns about.
    pub fn is_dry_run_query_error(&self, err: &duckdb::Error) -> bool {
        match self {
            ResolvedConnection::Bigquery { .. } => bigquery::is_dry_run_query_error(err),
        }
    }
}

/// Point Application Default Credentials at the profile-named key file before
/// any connection attaches. ADC is process-global (`GOOGLE_APPLICATION_CREDENTIALS`),
/// so every connection in a run must agree on the file; with no `credentials:`
/// set anywhere, the ambient chain applies (env var, gcloud login, workload
/// identity). Fails loud on a missing file — a deployed pod with an absent
/// secret mount should crash with this error, not limp into an auth failure.
pub fn prepare(sources: &IndexMap<String, ResolvedSource>, dir: &Path) -> Result<()> {
    let mut want: Option<(&str, String, &ResolvedConnection)> = None;
    for (name, src) in sources {
        let ResolvedSource::Connection { config, .. } = src else {
            continue;
        };
        let Some(path) = config.credentials() else {
            continue;
        };
        let resolved = resolve_credentials_path(path, dir);
        match &want {
            Some((first, existing, _)) if *existing != resolved => bail!(
                "sources '{first}' and '{name}' use connections with different credentials \
                 files ('{existing}' vs '{resolved}'); one run supports one ADC key file"
            ),
            Some(_) => {}
            None => want = Some((name, resolved, config)),
        }
    }
    if let Some((source, path, config)) = want {
        if !Path::new(&path).is_file() {
            bail!(
                "credentials file '{path}' (connection used by source '{source}') \
                 does not exist or is not a file"
            );
        }
        config.point_credentials_at(&path);
    }
    Ok(())
}

/// Relative credentials paths resolve against the cell directory, like
/// transforms and local bindings.
fn resolve_credentials_path(path: &str, dir: &Path) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        path.to_string()
    } else {
        dir.join(p).to_string_lossy().into_owned()
    }
}

/// Escape a double-quoted SQL identifier's inner content.
fn quote(ident: &str) -> String {
    ident.replace('"', "\"\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bq(project: &str, billing: Option<&str>, creds: Option<&str>) -> ResolvedConnection {
        ResolvedConnection::Bigquery {
            project: project.to_string(),
            billing_project: billing.map(str::to_string),
            credentials: creds.map(str::to_string),
            staging_uri: None,
        }
    }

    fn conn_source(config: ResolvedConnection) -> ResolvedSource {
        ResolvedSource::Connection {
            connection: "crm".to_string(),
            config,
            target: ConnectionTarget::Table("sales.accounts".to_string()),
            incremental: None,
        }
    }

    #[test]
    fn prepare_is_a_noop_without_credentials() {
        let mut sources = IndexMap::new();
        sources.insert("a".to_string(), conn_source(bq("p", None, None)));
        sources.insert(
            "raw".to_string(),
            ResolvedSource::Raw("s3://b/x.parquet".to_string()),
        );
        prepare(&sources, Path::new("/cell")).unwrap();
    }

    #[test]
    fn prepare_rejects_conflicting_credentials_files() {
        let mut sources = IndexMap::new();
        sources.insert(
            "a".to_string(),
            conn_source(bq("p", None, Some("/k1.json"))),
        );
        sources.insert(
            "b".to_string(),
            conn_source(bq("q", None, Some("/k2.json"))),
        );
        let err = prepare(&sources, Path::new("/cell"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("different credentials"), "got: {err}");
        assert!(err.contains("'a'") && err.contains("'b'"), "got: {err}");
    }

    #[test]
    fn prepare_fails_loud_on_a_missing_credentials_file() {
        let mut sources = IndexMap::new();
        sources.insert(
            "a".to_string(),
            conn_source(bq("p", None, Some("/definitely/not/there.json"))),
        );
        let err = prepare(&sources, Path::new("/cell"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "got: {err}");
    }

    #[test]
    fn credentials_paths_resolve_against_the_cell_dir() {
        assert_eq!(
            resolve_credentials_path("secrets/key.json", Path::new("/cell")),
            "/cell/secrets/key.json"
        );
        assert_eq!(
            resolve_credentials_path("/abs/key.json", Path::new("/cell")),
            "/abs/key.json"
        );
    }

    #[test]
    fn dataset_of_takes_the_first_segment() {
        assert_eq!(dataset_of("sales.accounts"), "sales");
        assert_eq!(dataset_of("nodot"), "nodot");
    }

    #[test]
    fn classify_cache_groups_siblings_by_connection_and_dataset() {
        let mut sources = IndexMap::new();
        sources.insert(
            "a".to_string(),
            ResolvedSource::Connection {
                connection: "crm".to_string(),
                config: bq("p", None, None),
                target: ConnectionTarget::Table("sales.accounts".to_string()),
                incremental: None,
            },
        );
        sources.insert(
            "b".to_string(),
            ResolvedSource::Connection {
                connection: "crm".to_string(),
                config: bq("p", None, None),
                target: ConnectionTarget::Table("sales.contacts".to_string()),
                incremental: None,
            },
        );
        sources.insert(
            "c".to_string(),
            ResolvedSource::Connection {
                connection: "crm".to_string(),
                config: bq("p", None, None),
                target: ConnectionTarget::Table("billing.invoices".to_string()),
                incremental: None,
            },
        );
        // ADR 0007: a sibling `query:` source on the same connection must
        // never enter the classification batch — it has no table, and
        // `bind_source` never calls `classify()` for it in the first place.
        sources.insert(
            "d".to_string(),
            ResolvedSource::Connection {
                connection: "crm".to_string(),
                config: bq("p", None, None),
                target: ConnectionTarget::Query("SELECT 1".to_string()),
                incremental: None,
            },
        );
        let cache = ClassifyCache::new(&sources);
        let mut sales = cache
            .siblings
            .get(&("crm".to_string(), "sales".to_string()))
            .unwrap()
            .clone();
        sales.sort();
        assert_eq!(sales, vec!["sales.accounts", "sales.contacts"]);
        assert_eq!(
            cache
                .siblings
                .get(&("crm".to_string(), "billing".to_string()))
                .unwrap(),
            &vec!["billing.invoices".to_string()]
        );
        // The `query:` source ("d") contributed no group at all — exactly
        // the two table-backed datasets, nothing keyed on the query text.
        assert_eq!(cache.siblings.len(), 2, "a query: source must not batch");
    }

    #[test]
    fn staging_uri_defaults_to_none_and_reads_back_when_set() {
        assert_eq!(bq("p", None, None).staging_uri(), None);
        let with_staging = ResolvedConnection::Bigquery {
            project: "p".to_string(),
            billing_project: None,
            credentials: None,
            staging_uri: Some("gs://acme-bq-staging/datamk-scratch".to_string()),
        };
        assert_eq!(
            with_staging.staging_uri(),
            Some("gs://acme-bq-staging/datamk-scratch")
        );
    }
}
