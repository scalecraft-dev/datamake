use super::schema::{
    is_valid_identifier, parse_duration, Bindings, CellDef, Connection, Incremental, Source,
};
use anyhow::{bail, Result};
use indexmap::IndexMap;

/// A cell's environment config with all `${VAR}` references expanded.
#[derive(Debug, Clone)]
pub struct ResolvedBindings {
    /// `Some` ⇒ direct attach; `None` ⇒ published-artifact mode (ADR 0004 §11).
    pub catalog: Option<String>,
    pub storage: String,
    pub s3: Option<ResolvedS3>,
    pub gcs: Option<ResolvedGcs>,
    /// Source name -> resolved source.
    pub sources: IndexMap<String, ResolvedSource>,
    /// Resolved path to the token->roles file, if configured.
    pub principals: Option<String>,
}

/// A source with env references expanded.
#[derive(Debug, Clone)]
pub enum ResolvedSource {
    Raw(String),
    Cell {
        /// `Some` ⇒ attach the upstream catalog directly; `None` ⇒ published
        /// mode against the upstream's storage prefix (ADR 0004 §12).
        catalog: Option<String>,
        storage: String,
        table: String,
        version: Option<u64>,
    },
    /// A warehouse object with its connection config inlined (as `Cell`
    /// inlines its location). `connection` keeps the reference name for the
    /// engine's attach alias, shared by every source on the same connection.
    Connection {
        connection: String,
        config: ResolvedConnection,
        /// A table path or author-owned query (ADR 0007) — exactly one,
        /// established by construction below.
        target: ConnectionTarget,
        /// Watermarked-read config (ADR 0005), resolved: cursor expanded and
        /// identifier-validated, lookback parsed to a `Duration`. Consumed by
        /// the engine's incremental bind path (ADR 0005 Stage 2). Always
        /// `None` when `target` is `Query` — refused at resolve time (ADR
        /// 0007 §3).
        incremental: Option<ResolvedIncremental>,
    },
}

/// What a `connection` source reads (ADR 0007): a warehouse table path,
/// routed by object-kind classification (ADR 0006) — or author-owned
/// server-side SQL, routed to the jobs API by construction (no
/// classification, no `qualify()`). Exactly one, established here from
/// `Source::Connection`'s exactly-one-of `table`/`query` (enforced earlier,
/// at parse time, by `deserialize_connection`) — every downstream consumer
/// matches this enum instead of re-checking an `Option` pair.
#[derive(Debug, Clone)]
pub enum ConnectionTarget {
    Table(String),
    Query(String),
}

/// A resolved `incremental:` block (ADR 0005). Cursor existence/type/
/// nullability against the live warehouse column is bind-time only — this
/// resolve-time validation covers identifier shape and duration parsing, the
/// two things checkable offline.
#[derive(Debug, Clone)]
pub struct ResolvedIncremental {
    pub cursor: String,
    pub lookback: Option<std::time::Duration>,
}

/// A warehouse connection with env references expanded.
#[derive(Debug, Clone)]
pub enum ResolvedConnection {
    Bigquery {
        project: String,
        billing_project: Option<String>,
        credentials: Option<String>,
        /// ADR 0006 §3a: scratch object-store prefix for the oversized-
        /// jobs-result escape hatch. `None` ⇒ an oversized result is a hard
        /// error naming this field.
        staging_uri: Option<String>,
    },
    Snowflake {
        account: String,
        /// The database whose schemas are read — the environment root,
        /// analog of BigQuery's `project`.
        database: String,
        auth: SnowflakeAuth,
        warehouse: Option<String>,
        role: Option<String>,
    },
}

/// How a Snowflake connection authenticates — exactly one mechanism,
/// established at resolve time (`resolve_snowflake`).
#[derive(Debug, Clone)]
pub enum SnowflakeAuth {
    /// A service account's PKCS#8 key file — the deployed/prod shape.
    KeyPair {
        user: String,
        private_key_path: String,
        passphrase: Option<Redacted>,
    },
    /// SSO through the user's own browser — the interactive local-dev
    /// shape; refused by deploy pre-flight (a pod has no browser). `user`
    /// is required: the extension refuses a secret without it
    /// (live-verified: "Snowflake secret requires field 'user'").
    ExternalBrowser { user: String },
}

/// A secret string whose value must never reach logs or error text —
/// `Debug` (what `{:?}` on any containing type prints) is redacted.
#[derive(Clone)]
pub struct Redacted(pub String);

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl ResolvedSource {
    /// The location this source reads (`None` for connection sources, which go
    /// through a scanner extension, not httpfs). May be a local path for raw
    /// sources — check with `is_s3`/`is_gcs`/`is_remote`; a remote location
    /// needs httpfs + that scheme's secret.
    pub fn location(&self) -> Option<&str> {
        match self {
            ResolvedSource::Raw(uri) => Some(uri),
            ResolvedSource::Cell { storage, .. } => Some(storage),
            ResolvedSource::Connection { .. } => None,
        }
    }
}

/// Whether a URI is on S3 (needs httpfs + a `TYPE s3` secret).
pub fn is_s3(uri: &str) -> bool {
    uri.starts_with("s3://")
}

/// Whether a URI is on Google Cloud Storage (needs httpfs + a `TYPE gcs`
/// secret). Both spellings, matching DuckDB's own secret scope.
pub fn is_gcs(uri: &str) -> bool {
    uri.starts_with("gs://") || uri.starts_with("gcs://")
}

/// Whether a URI points at object storage (`s3://`/`gs://`/`gcs://`), and so
/// needs the httpfs extension + a store secret. The inverse — a local/file
/// path — is what deploy pre-flight refuses for a remote workload.
pub fn is_remote(uri: &str) -> bool {
    is_s3(uri) || is_gcs(uri)
}

/// Whether a catalog DSN is metadata-DB-backed (`sqlite:`/`postgres:`), as opposed
/// to a DuckDB-file `.ducklake` catalog. A metadata-DB catalog is concurrent-safe
/// (Server and Builder can attach at once); deploy requires it.
pub fn is_metadata_db_catalog(catalog: &str) -> bool {
    catalog.starts_with("sqlite:") || catalog.starts_with("postgres:")
}

#[derive(Debug, Clone)]
pub struct ResolvedS3 {
    pub region: Option<String>,
    pub endpoint: Option<String>,
    pub url_style: Option<String>,
    pub key_id: Option<String>,
    pub secret: Option<String>,
    pub session_token: Option<String>,
    pub use_ssl: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ResolvedGcs {
    /// Absolute path to a service-account key file (relative paths are
    /// resolved against the cell directory by `config::load`).
    pub credentials: Option<String>,
    /// Absolute path to a native GCS DuckDB extension (resolved like
    /// `credentials`). Set ⇒ DuckDB authenticates with OAuth/ADC, no HMAC.
    pub extension: Option<String>,
    pub key_id: Option<String>,
    pub secret: Option<String>,
    pub endpoint: Option<String>,
    pub use_ssl: Option<bool>,
}

/// Resolve a cell's sources (from `cell.yaml`) against a binding profile (from
/// `profiles/<name>.yaml`), expanding all `${VAR}` references.
pub fn resolve(def: &CellDef, b: &Bindings) -> Result<ResolvedBindings> {
    let s3 = match &b.s3 {
        Some(s) => Some(ResolvedS3 {
            region: expand_opt(&s.region)?,
            endpoint: expand_opt(&s.endpoint)?,
            url_style: expand_opt(&s.url_style)?,
            key_id: expand_opt(&s.key_id)?,
            secret: expand_opt(&s.secret)?,
            session_token: expand_opt(&s.session_token)?,
            use_ssl: s.use_ssl,
        }),
        None => None,
    };
    let gcs = match &b.gcs {
        Some(g) => Some(ResolvedGcs {
            credentials: expand_opt(&g.credentials)?,
            extension: expand_opt(&g.extension)?,
            key_id: expand_opt(&g.key_id)?,
            secret: expand_opt(&g.secret)?,
            endpoint: expand_opt(&g.endpoint)?,
            use_ssl: g.use_ssl,
        }),
        None => None,
    };

    let mut sources = IndexMap::new();
    for (name, src) in &def.sources {
        let resolved = match src {
            Source::Raw(uri) => ResolvedSource::Raw(expand(uri)?),
            Source::Cell {
                cell,
                table,
                version,
            } => {
                let loc = b.cells.get(cell).ok_or_else(|| {
                    anyhow::anyhow!(
                        "source '{name}' depends on cell '{cell}', but the profile has no \
                         `cells.{cell}` location"
                    )
                })?;
                ResolvedSource::Cell {
                    catalog: expand_opt(&loc.catalog)?,
                    storage: expand(&loc.storage)?,
                    table: expand(table)?,
                    version: *version,
                }
            }
            Source::Connection {
                connection,
                table,
                query,
                incremental,
            } => {
                let conn = b.connections.get(connection).ok_or_else(|| {
                    anyhow::anyhow!(
                        "source '{name}' uses connection '{connection}', but the profile has no \
                         `connections.{connection}` entry"
                    )
                })?;
                let resolved_conn = resolve_connection(connection, conn)?;
                // ADR 0007 §3: the shipped watermark mechanics append
                // `WHERE cursor > <literal>` to an engine-generated
                // `SELECT *` — a syntax error after an author's `GROUP BY`,
                // or an implicit (and semantically load-bearing)
                // pre-vs-post-aggregation choice if wrapped around the
                // query instead. Refused here, at resolve time, rather than
                // silently doing the wrong one.
                if query.is_some() && incremental.is_some() {
                    bail!(
                        "source '{name}': `incremental:` is not yet supported on a `query:` \
                         source. Use `table:` with `incremental:`, or drop `incremental:` — a \
                         `query:` source is re-read in full each run."
                    );
                }
                let incremental = incremental
                    .as_ref()
                    .map(|inc| resolve_incremental(name, inc))
                    .transpose()?;
                // Both/neither are already impossible past
                // `deserialize_connection` (ADR 0007 §1 — exactly-one-of,
                // enforced at parse time); the fallback arm below is
                // defensive, not a path normal parsing can reach.
                let target = match (table, query) {
                    (Some(t), None) => ConnectionTarget::Table(expand(t)?),
                    (None, Some(q)) => {
                        // ADR 0007 §1: `${connection.project}` is a reserved,
                        // engine-owned binding, substituted before ordinary
                        // `${VAR}` expansion so `expand()` never sees the
                        // `connection.` prefix (env-var names cannot contain
                        // `.`, so there is no collision either way).
                        let substituted = substitute_connection_bindings(
                            name,
                            q,
                            connection_project(&resolved_conn),
                        )?;
                        ConnectionTarget::Query(expand(&substituted)?)
                    }
                    _ => bail!(
                        "source '{name}': a connection source must have exactly one of `table` \
                         or `query` (internal error — this should have been caught at parse \
                         time) — please report this"
                    ),
                };
                ResolvedSource::Connection {
                    connection: connection.clone(),
                    config: resolved_conn,
                    target,
                    incremental,
                }
            }
        };
        sources.insert(name.clone(), resolved);
    }

    let principals = expand_opt(&b.principals)?;
    let storage = expand(&b.storage)?;

    // A profile whose storage moved to GCS but whose credentials didn't is a
    // config smell worth naming — but only a warning: `s3://` sources beside
    // `gs://` storage is a legitimate mix.
    if is_gcs(&storage) && s3.is_some() && gcs.is_none() {
        tracing::warn!(
            "profile has an `s3:` block but storage '{storage}' is Google Cloud Storage; \
             s3 settings don't apply to gs:// — add a `gcs:` block"
        );
    }

    Ok(ResolvedBindings {
        catalog: expand_opt(&b.catalog)?,
        storage,
        s3,
        gcs,
        sources,
        principals,
    })
}

/// Resolve-time validation for an `incremental:` block (ADR 0005 §1): expand
/// `${VAR}` in the cursor like `table`, then validate its identifier shape
/// (defense in depth — it is double-quoted at the SQL build site anyway) and
/// parse `lookback`. Cursor existence/type/nullability against the live
/// warehouse column can only be checked at bind time.
fn resolve_incremental(source_name: &str, inc: &Incremental) -> Result<ResolvedIncremental> {
    let cursor = expand(&inc.cursor)?;
    if !is_valid_identifier(&cursor) {
        bail!(
            "source '{source_name}': incremental cursor '{cursor}' is not a valid column \
             identifier — use a bare column name matching [A-Za-z_][A-Za-z0-9_]* (no dots, \
             quotes, or expressions)"
        );
    }
    let lookback = match &inc.lookback {
        Some(s) => {
            let expanded = expand(s)?;
            Some(
                parse_duration(&expanded)
                    .map_err(|e| anyhow::anyhow!("source '{source_name}': {e}"))?,
            )
        }
        None => None,
    };
    Ok(ResolvedIncremental { cursor, lookback })
}

fn resolve_connection(name: &str, c: &Connection) -> Result<ResolvedConnection> {
    match c {
        Connection::Bigquery(bq) => super::connections::bigquery::resolve_bigquery(bq),
        Connection::Snowflake(sf) => super::connections::snowflake::resolve_snowflake(name, sf),
    }
}

/// The project `${connection.project}` substitutes (ADR 0007 §1). A free
/// function rather than a `ResolvedConnection` method: "project" is a
/// BigQuery-shaped concept, and `query:` sources are a BigQuery-shaped
/// feature today — a future non-BigQuery connector shouldn't be forced to
/// answer "what's your project" just because this one binding needs it.
/// `None` ⇒ the connector has no such binding (Snowflake: the session's
/// database already qualifies unqualified names, so a `query:` body needs
/// no placeholder at all — `${connection.project}` in one is an error
/// naming that fact).
fn connection_project(c: &ResolvedConnection) -> Option<&str> {
    match c {
        ResolvedConnection::Bigquery { project, .. } => Some(project),
        ResolvedConnection::Snowflake { .. } => None,
    }
}

/// The reserved `${connection.*}` prefix (ADR 0007 §1): engine-owned, never
/// an env var — env-var names cannot contain `.`, so there is no collision
/// with `expand()`'s `${VAR}` syntax either way.
const CONNECTION_BINDING_PREFIX: &str = "connection.";

/// Substitute the reserved `${connection.project}` binding in a `query:`
/// body with `project` — the source's *resolved* connection project. This is
/// the `query:` analog of what `qualify()` does for `table:`: the engine
/// owns qualification of the reference, so the project value never appears
/// in the contract and a dev/prod split just works.
///
/// *Why this exists, learned live:* an unqualified `dataset.table` in a
/// BigQuery job resolves against the project the job **runs in**, which
/// under split billing (`billing_project` set, and different from
/// `project`) is the billing project, not the storage project — so a bare
/// reference silently resolved against the wrong project and failed with
/// `Not found: Dataset <billing>:<dataset>`. An author who forgets this
/// placeholder still gets a loud bind-time error (a genuinely bare
/// `dataset.table` fails the same way it always did); an author who
/// hardcodes a project instead leaks environment into the contract
/// undetected, which review must catch — the engine cannot parse GoogleSQL
/// to stop it.
///
/// Applied to `query:` bodies only, before `expand()`, so ordinary
/// `${ENV_VAR}` substitution never has to special-case the `connection.`
/// prefix. Any `${connection.*}` name other than `project` is a resolve-time
/// error naming the one supported binding.
fn substitute_connection_bindings(
    source_name: &str,
    query: &str,
    project: Option<&str>,
) -> Result<String> {
    let mut out = String::new();
    let mut i = 0;
    while i < query.len() {
        if query[i..].starts_with("${connection.") {
            let end = query[i + 2..].find('}').map(|p| i + 2 + p).ok_or_else(|| {
                anyhow::anyhow!(
                    "source '{source_name}': unterminated ${{connection...}} in `query:`"
                )
            })?;
            let name = &query[i + 2 + CONNECTION_BINDING_PREFIX.len()..end];
            match (name, project) {
                ("project", Some(p)) => out.push_str(p),
                ("project", None) => bail!(
                    "source '{source_name}': `${{connection.project}}` does not apply to this \
                     connection type — a snowflake `query:` runs with the connection's \
                     `database` as the session database, so unqualified `schema.table` names \
                     already resolve against it; remove the placeholder."
                ),
                // The advice must match the connector: naming
                // `${connection.project}` as "the one supported binding" to
                // a connector that rejects it (the arm above) would steer
                // the author into a second guaranteed failure.
                (other, Some(_)) => bail!(
                    "source '{source_name}': `${{connection.{other}}}` is not a supported \
                     binding in `query:` — the only supported binding is \
                     `${{connection.project}}`."
                ),
                (other, None) => bail!(
                    "source '{source_name}': `${{connection.{other}}}` is not a supported \
                     binding in `query:` — no `${{connection.*}}` binding applies to this \
                     connection type; a snowflake `query:` runs with the connection's \
                     `database` as the session database, so unqualified `schema.table` names \
                     already resolve against it."
                ),
            }
            i = end + 1;
        } else {
            let ch = query[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

pub(crate) fn expand_opt(o: &Option<String>) -> Result<Option<String>> {
    match o {
        Some(s) => {
            let e = expand(s)?;
            Ok((!e.is_empty()).then_some(e))
        }
        None => Ok(None),
    }
}

/// Expand `${VAR}` and `${VAR:-default}` from the environment.
pub fn expand(input: &str) -> Result<String> {
    let mut out = String::new();
    let mut i = 0;
    while i < input.len() {
        if input[i..].starts_with("${") {
            let end = input[i + 2..]
                .find('}')
                .map(|p| i + 2 + p)
                .ok_or_else(|| anyhow::anyhow!("unterminated ${{...}} in '{input}'"))?;
            let (var, default) = match input[i + 2..end].split_once(":-") {
                Some((v, d)) => (v, Some(d)),
                None => (&input[i + 2..end], None),
            };
            match std::env::var(var) {
                Ok(val) => out.push_str(&val),
                Err(_) => match default {
                    Some(d) => out.push_str(d),
                    None => bail!("env var '{var}' unset and has no default"),
                },
            }
            i = end + 1;
        } else {
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::schema::{
        BigQueryConnection, Bindings, CellLocation, Export, GcsBinding, S3Binding,
        SnowflakeConnection,
    };
    use super::*;

    #[test]
    fn expand_passes_literals_through() {
        assert_eq!(
            expand("plain text, no vars").unwrap(),
            "plain text, no vars"
        );
        assert_eq!(expand("").unwrap(), "");
    }

    #[test]
    fn expand_substitutes_a_set_var() {
        std::env::set_var("DATAMK_TEST_EXPAND_SET", "world");
        assert_eq!(
            expand("hello ${DATAMK_TEST_EXPAND_SET}!").unwrap(),
            "hello world!"
        );
    }

    #[test]
    fn expand_uses_default_when_var_unset() {
        // A var name that is (almost certainly) never set in the environment.
        assert_eq!(
            expand("${DATAMK_TEST_UNSET_XYZ:-fallback}").unwrap(),
            "fallback"
        );
    }

    #[test]
    fn expand_empty_default_yields_empty() {
        assert_eq!(expand("${DATAMK_TEST_UNSET_EMPTY:-}").unwrap(), "");
    }

    #[test]
    fn expand_prefers_set_var_over_default() {
        std::env::set_var("DATAMK_TEST_EXPAND_PREF", "real");
        assert_eq!(
            expand("${DATAMK_TEST_EXPAND_PREF:-fallback}").unwrap(),
            "real"
        );
    }

    #[test]
    fn expand_errors_on_unset_without_default() {
        let err = expand("${DATAMK_TEST_DEFINITELY_UNSET}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unset"), "unexpected error: {err}");
    }

    #[test]
    fn expand_errors_on_unterminated() {
        let err = expand("oops ${VAR").unwrap_err().to_string();
        assert!(err.contains("unterminated"), "unexpected error: {err}");
    }

    #[test]
    fn expand_handles_multiple_and_unicode() {
        std::env::set_var("DATAMK_TEST_A", "1");
        std::env::set_var("DATAMK_TEST_B", "2");
        assert_eq!(
            expand("café ${DATAMK_TEST_A}-${DATAMK_TEST_B} ✓").unwrap(),
            "café 1-2 ✓"
        );
    }

    #[test]
    fn expand_opt_treats_empty_result_as_none() {
        assert_eq!(expand_opt(&None).unwrap(), None);
        assert_eq!(expand_opt(&Some("".to_string())).unwrap(), None);
        assert_eq!(
            expand_opt(&Some("${DATAMK_TEST_UNSET_OPT:-}".to_string())).unwrap(),
            None
        );
        assert_eq!(
            expand_opt(&Some("value".to_string())).unwrap(),
            Some("value".to_string())
        );
    }

    fn bindings_with_cell(cell: &str, loc: CellLocation) -> Bindings {
        let mut cells = IndexMap::new();
        cells.insert(cell.to_string(), loc);
        Bindings {
            catalog: Some("./cat.ducklake".to_string()),
            storage: "./data".to_string(),
            s3: None,
            gcs: None,
            principals: None,
            cells,
            connections: IndexMap::new(),
        }
    }

    fn cell_with_source(name: &str, src: Source) -> CellDef {
        let mut sources = IndexMap::new();
        sources.insert(name.to_string(), src);
        CellDef {
            cell: "c".to_string(),
            sources,
            transforms: vec![],
            interface: vec![] as Vec<Export>,
            access: Default::default(),
        }
    }

    #[test]
    fn resolve_passes_through_a_raw_source() {
        let def = cell_with_source("raw", Source::Raw("s3://bucket/x.parquet".to_string()));
        let b = Bindings {
            catalog: Some("c".into()),
            storage: "s".into(),
            s3: None,
            gcs: None,
            principals: None,
            cells: IndexMap::new(),
            connections: IndexMap::new(),
        };
        let r = resolve(&def, &b).unwrap();
        match r.sources.get("raw").unwrap() {
            ResolvedSource::Raw(uri) => assert_eq!(uri, "s3://bucket/x.parquet"),
            other => panic!("expected raw, got {other:?}"),
        }
    }

    #[test]
    fn resolve_links_a_cell_source_via_the_profile() {
        let def = cell_with_source(
            "upstream",
            Source::Cell {
                cell: "other".to_string(),
                table: "orders".to_string(),
                version: Some(7),
            },
        );
        let b = bindings_with_cell(
            "other",
            CellLocation {
                catalog: Some("/lake/other.ducklake".to_string()),
                storage: "/lake/other/data".to_string(),
            },
        );
        let r = resolve(&def, &b).unwrap();
        match r.sources.get("upstream").unwrap() {
            ResolvedSource::Cell {
                catalog,
                storage,
                table,
                version,
            } => {
                assert_eq!(catalog.as_deref(), Some("/lake/other.ducklake"));
                assert_eq!(storage, "/lake/other/data");
                assert_eq!(table, "orders");
                assert_eq!(*version, Some(7));
            }
            other => panic!("expected cell, got {other:?}"),
        }
    }

    #[test]
    fn resolve_errors_when_cell_location_is_missing() {
        let def = cell_with_source(
            "upstream",
            Source::Cell {
                cell: "missing".to_string(),
                table: "t".to_string(),
                version: None,
            },
        );
        let b = Bindings {
            catalog: Some("c".into()),
            storage: "s".into(),
            s3: None,
            gcs: None,
            principals: None,
            cells: IndexMap::new(),
            connections: IndexMap::new(),
        };
        let err = resolve(&def, &b).unwrap_err().to_string();
        assert!(err.contains("missing"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_links_a_connection_source_via_the_profile() {
        std::env::set_var("DATAMK_TEST_BQ_PROJECT", "acme-prod-crm");
        let def = cell_with_source(
            "crm_accounts",
            Source::Connection {
                connection: "crm".to_string(),
                table: Some("sales.accounts".to_string()),
                query: None,
                incremental: None,
            },
        );
        let mut connections = IndexMap::new();
        connections.insert(
            "crm".to_string(),
            Connection::Bigquery(BigQueryConnection {
                project: "${DATAMK_TEST_BQ_PROJECT}".to_string(),
                billing_project: None,
                credentials: None,
                staging_uri: None,
            }),
        );
        let b = Bindings {
            catalog: Some("c".into()),
            storage: "s".into(),
            s3: None,
            gcs: None,
            principals: None,
            cells: IndexMap::new(),
            connections,
        };
        let r = resolve(&def, &b).unwrap();
        let src = r.sources.get("crm_accounts").unwrap();
        // A connection source reads through a scanner extension, not httpfs.
        assert!(src.location().is_none());
        match src {
            ResolvedSource::Connection {
                connection,
                config: ResolvedConnection::Bigquery { project, .. },
                target,
                incremental,
            } => {
                assert_eq!(connection, "crm");
                assert_eq!(project, "acme-prod-crm");
                assert!(matches!(target, ConnectionTarget::Table(t) if t == "sales.accounts"));
                assert!(incremental.is_none());
            }
            other => panic!("expected connection, got {other:?}"),
        }
    }

    #[test]
    fn resolve_expands_staging_uri_and_leaves_it_none_when_unset() {
        std::env::set_var(
            "DATAMK_TEST_BQ_STAGING",
            "gs://acme-bq-staging/datamk-scratch",
        );
        let def = cell_with_source(
            "crm_accounts",
            Source::Connection {
                connection: "crm".to_string(),
                table: Some("sales.accounts".to_string()),
                query: None,
                incremental: None,
            },
        );
        let mut connections = IndexMap::new();
        connections.insert(
            "crm".to_string(),
            Connection::Bigquery(BigQueryConnection {
                project: "acme-prod-crm".to_string(),
                billing_project: None,
                credentials: None,
                staging_uri: Some("${DATAMK_TEST_BQ_STAGING}".to_string()),
            }),
        );
        let b = Bindings {
            catalog: Some("c".into()),
            storage: "s".into(),
            s3: None,
            gcs: None,
            principals: None,
            cells: IndexMap::new(),
            connections,
        };
        let r = resolve(&def, &b).unwrap();
        match r.sources.get("crm_accounts").unwrap() {
            ResolvedSource::Connection {
                config: ResolvedConnection::Bigquery { staging_uri, .. },
                ..
            } => {
                assert_eq!(
                    staging_uri.as_deref(),
                    Some("gs://acme-bq-staging/datamk-scratch")
                );
            }
            other => panic!("expected connection, got {other:?}"),
        }

        // Unset entirely (the common case) resolves to None, not an error.
        let r2 = resolve(&def, &bindings_with_crm_connection()).unwrap();
        match r2.sources.get("crm_accounts").unwrap() {
            ResolvedSource::Connection {
                config: ResolvedConnection::Bigquery { staging_uri, .. },
                ..
            } => assert_eq!(*staging_uri, None),
            other => panic!("expected connection, got {other:?}"),
        }
    }

    #[test]
    fn resolve_errors_when_connection_is_missing() {
        let def = cell_with_source(
            "crm_accounts",
            Source::Connection {
                connection: "crm".to_string(),
                table: Some("sales.accounts".to_string()),
                query: None,
                incremental: None,
            },
        );
        let b = Bindings {
            catalog: Some("c".into()),
            storage: "s".into(),
            s3: None,
            gcs: None,
            principals: None,
            cells: IndexMap::new(),
            connections: IndexMap::new(),
        };
        let err = resolve(&def, &b).unwrap_err().to_string();
        assert!(err.contains("connections.crm"), "unexpected error: {err}");
        assert!(err.contains("crm_accounts"), "unexpected error: {err}");
    }

    fn bindings_with_crm_connection() -> Bindings {
        let mut connections = IndexMap::new();
        connections.insert(
            "crm".to_string(),
            Connection::Bigquery(BigQueryConnection {
                project: "acme-prod-crm".to_string(),
                billing_project: None,
                credentials: None,
                staging_uri: None,
            }),
        );
        Bindings {
            catalog: Some("c".into()),
            storage: "s".into(),
            s3: None,
            gcs: None,
            principals: None,
            cells: IndexMap::new(),
            connections,
        }
    }

    #[test]
    fn resolve_incremental_cursor_only() {
        let def = cell_with_source(
            "crm_accounts",
            Source::Connection {
                connection: "crm".to_string(),
                table: Some("sales.accounts".to_string()),
                query: None,
                incremental: Some(Incremental {
                    cursor: "updated_at".to_string(),
                    lookback: None,
                }),
            },
        );
        let r = resolve(&def, &bindings_with_crm_connection()).unwrap();
        match r.sources.get("crm_accounts").unwrap() {
            ResolvedSource::Connection { incremental, .. } => {
                let inc = incremental.as_ref().unwrap();
                assert_eq!(inc.cursor, "updated_at");
                assert!(inc.lookback.is_none());
            }
            other => panic!("expected connection, got {other:?}"),
        }
    }

    #[test]
    fn resolve_incremental_cursor_and_lookback() {
        let def = cell_with_source(
            "crm_accounts",
            Source::Connection {
                connection: "crm".to_string(),
                table: Some("sales.accounts".to_string()),
                query: None,
                incremental: Some(Incremental {
                    cursor: "updated_at".to_string(),
                    lookback: Some("2h".to_string()),
                }),
            },
        );
        let r = resolve(&def, &bindings_with_crm_connection()).unwrap();
        match r.sources.get("crm_accounts").unwrap() {
            ResolvedSource::Connection { incremental, .. } => {
                let inc = incremental.as_ref().unwrap();
                assert_eq!(inc.cursor, "updated_at");
                assert_eq!(inc.lookback.unwrap().as_secs(), 7200);
            }
            other => panic!("expected connection, got {other:?}"),
        }
    }

    // --- ADR 0007: `query:` connection sources ------------------------------

    #[test]
    fn resolve_threads_a_query_source_into_connectiontarget_query() {
        let def = cell_with_source(
            "raw_spend_hourly",
            Source::Connection {
                connection: "crm".to_string(),
                table: None,
                query: Some("SELECT 1 AS x".to_string()),
                incremental: None,
            },
        );
        let r = resolve(&def, &bindings_with_crm_connection()).unwrap();
        match r.sources.get("raw_spend_hourly").unwrap() {
            ResolvedSource::Connection {
                target,
                incremental,
                ..
            } => {
                assert!(
                    matches!(target, ConnectionTarget::Query(q) if q == "SELECT 1 AS x"),
                    "expected ConnectionTarget::Query, got {target:?}"
                );
                assert!(incremental.is_none());
            }
            other => panic!("expected connection, got {other:?}"),
        }
    }

    #[test]
    fn resolve_expands_env_vars_inside_a_query_source() {
        std::env::set_var("DATAMK_TEST_QUERY_DATASET", "summarydata");
        let def = cell_with_source(
            "raw_spend_hourly",
            Source::Connection {
                connection: "crm".to_string(),
                table: None,
                query: Some(
                    "SELECT 1 FROM `${DATAMK_TEST_QUERY_DATASET}.campaign_group_spend_by_minute`"
                        .to_string(),
                ),
                incremental: None,
            },
        );
        let r = resolve(&def, &bindings_with_crm_connection()).unwrap();
        match r.sources.get("raw_spend_hourly").unwrap() {
            ResolvedSource::Connection { target, .. } => match target {
                ConnectionTarget::Query(q) => {
                    assert!(
                        q.contains("summarydata.campaign_group_spend_by_minute"),
                        "got: {q}"
                    );
                }
                other => panic!("expected Query target, got {other:?}"),
            },
            other => panic!("expected connection, got {other:?}"),
        }
    }

    /// A connection whose `billing_project` differs from `project` — the
    /// split-billing shape that live-proved the `${connection.project}` bug
    /// (flight-spend's real profile: `project: dw-main-silver`,
    /// `billing_project: dw-main-bronze`). The fix under test is that
    /// substitution always inserts `project` (the storage project), never
    /// `billing_project` (the job/billing project) — the two must not be
    /// conflated.
    fn bindings_with_split_billing_connection() -> Bindings {
        let mut connections = IndexMap::new();
        connections.insert(
            "crm".to_string(),
            Connection::Bigquery(BigQueryConnection {
                project: "dw-main-silver".to_string(),
                billing_project: Some("dw-main-bronze".to_string()),
                credentials: None,
                staging_uri: None,
            }),
        );
        Bindings {
            catalog: Some("c".into()),
            storage: "s".into(),
            s3: None,
            gcs: None,
            principals: None,
            cells: IndexMap::new(),
            connections,
        }
    }

    #[test]
    fn resolve_substitutes_connection_project_in_a_query_source() {
        let def = cell_with_source(
            "raw_spend_hourly",
            Source::Connection {
                connection: "crm".to_string(),
                table: None,
                query: Some(
                    "SELECT 1 FROM `${connection.project}.summarydata.t` GROUP BY 1".to_string(),
                ),
                incremental: None,
            },
        );
        let r = resolve(&def, &bindings_with_crm_connection()).unwrap();
        match r.sources.get("raw_spend_hourly").unwrap() {
            ResolvedSource::Connection { target, .. } => match target {
                ConnectionTarget::Query(q) => {
                    assert_eq!(q, "SELECT 1 FROM `acme-prod-crm.summarydata.t` GROUP BY 1")
                }
                other => panic!("expected Query target, got {other:?}"),
            },
            other => panic!("expected connection, got {other:?}"),
        }
    }

    #[test]
    fn resolve_substitutes_the_storage_project_not_the_billing_project() {
        // The live bug: unqualified `dataset.table` resolves against the
        // job's (billing) project under split billing. The fix must always
        // render `project` (dw-main-silver), never `billing_project`
        // (dw-main-bronze) — substitution is a storage-qualification
        // concern, not a billing one.
        let def = cell_with_source(
            "raw_spend_hourly",
            Source::Connection {
                connection: "crm".to_string(),
                table: None,
                query: Some(
                    "SELECT * FROM `${connection.project}.summarydata.campaign_group_spend_by_minute`"
                        .to_string(),
                ),
                incremental: None,
            },
        );
        let r = resolve(&def, &bindings_with_split_billing_connection()).unwrap();
        match r.sources.get("raw_spend_hourly").unwrap() {
            ResolvedSource::Connection { target, .. } => match target {
                ConnectionTarget::Query(q) => {
                    assert!(
                        q.contains("`dw-main-silver.summarydata.campaign_group_spend_by_minute`"),
                        "must render the storage project, not the billing project: {q}"
                    );
                    assert!(
                        !q.contains("dw-main-bronze"),
                        "must never render the billing project: {q}"
                    );
                }
                other => panic!("expected Query target, got {other:?}"),
            },
            other => panic!("expected connection, got {other:?}"),
        }
    }

    #[test]
    fn resolve_rejects_an_unsupported_connection_dot_binding_naming_the_one_supported_one() {
        let def = cell_with_source(
            "raw_spend_hourly",
            Source::Connection {
                connection: "crm".to_string(),
                table: None,
                query: Some("SELECT * FROM `${connection.billing_project}.x.y`".to_string()),
                incremental: None,
            },
        );
        let err = resolve(&def, &bindings_with_split_billing_connection())
            .unwrap_err()
            .to_string();
        assert!(err.contains("raw_spend_hourly"), "{err}");
        assert!(
            err.contains("`${connection.billing_project}`"),
            "must name the offending binding: {err}"
        );
        assert!(
            err.contains("`${connection.project}`"),
            "must name the one supported binding: {err}"
        );
    }

    fn bindings_with_snowflake_connection() -> Bindings {
        let mut connections = IndexMap::new();
        connections.insert(
            "wh".to_string(),
            Connection::Snowflake(SnowflakeConnection {
                account: "MYORG-ACCT".to_string(),
                user: Some("SVC_USER".to_string()),
                database: "ANALYTICS".to_string(),
                private_key_path: Some("/keys/sf.p8".to_string()),
                private_key_passphrase: None,
                authenticator: None,
                warehouse: Some("WH".to_string()),
                role: None,
                password: None,
            }),
        );
        Bindings {
            catalog: Some("c".into()),
            storage: "s".into(),
            s3: None,
            gcs: None,
            principals: None,
            cells: IndexMap::new(),
            connections,
        }
    }

    #[test]
    fn resolve_links_a_snowflake_connection_source_via_the_profile() {
        let def = cell_with_source(
            "models",
            Source::Connection {
                connection: "wh".to_string(),
                table: Some("raw.vehicle_models".to_string()),
                query: None,
                incremental: None,
            },
        );
        let r = resolve(&def, &bindings_with_snowflake_connection()).unwrap();
        match r.sources.get("models").unwrap() {
            ResolvedSource::Connection {
                connection,
                config:
                    ResolvedConnection::Snowflake {
                        account, database, ..
                    },
                target,
                ..
            } => {
                assert_eq!(connection, "wh");
                assert_eq!(account, "MYORG-ACCT");
                assert_eq!(database, "ANALYTICS");
                assert!(matches!(target, ConnectionTarget::Table(t) if t == "raw.vehicle_models"));
            }
            other => panic!("expected snowflake connection, got {other:?}"),
        }
    }

    #[test]
    fn resolve_rejects_connection_project_in_a_snowflake_query_source() {
        let def = cell_with_source(
            "spend",
            Source::Connection {
                connection: "wh".to_string(),
                table: None,
                query: Some("SELECT * FROM `${connection.project}.x.y`".to_string()),
                incremental: None,
            },
        );
        let err = resolve(&def, &bindings_with_snowflake_connection())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("does not apply to this connection type"),
            "{err}"
        );
        assert!(err.contains("session database"), "{err}");
    }

    #[test]
    fn resolve_rejects_other_connection_bindings_on_snowflake_without_bad_advice() {
        // The generic unsupported-binding error names `${connection.project}`
        // as the fix — advice that is itself rejected on snowflake, so this
        // connector gets the no-bindings-apply explanation instead.
        let def = cell_with_source(
            "spend",
            Source::Connection {
                connection: "wh".to_string(),
                table: None,
                query: Some("SELECT * FROM ${connection.database}.raw.t".to_string()),
                incremental: None,
            },
        );
        let err = resolve(&def, &bindings_with_snowflake_connection())
            .unwrap_err()
            .to_string();
        assert!(err.contains("`${connection.database}`"), "{err}");
        assert!(
            err.contains("no `${connection.*}` binding applies"),
            "{err}"
        );
        assert!(
            !err.contains("the only supported binding is"),
            "must not advise `${{connection.project}}` on snowflake: {err}"
        );
    }

    #[test]
    fn resolve_accepts_a_snowflake_query_source_with_no_bindings() {
        let def = cell_with_source(
            "spend",
            Source::Connection {
                connection: "wh".to_string(),
                table: None,
                query: Some("SELECT model_id FROM RAW.VEHICLE_MODELS".to_string()),
                incremental: None,
            },
        );
        let r = resolve(&def, &bindings_with_snowflake_connection()).unwrap();
        match r.sources.get("spend").unwrap() {
            ResolvedSource::Connection { target, .. } => {
                assert!(matches!(
                    target,
                    ConnectionTarget::Query(q) if q == "SELECT model_id FROM RAW.VEHICLE_MODELS"
                ));
            }
            other => panic!("expected connection, got {other:?}"),
        }
    }

    #[test]
    fn resolve_expands_plain_env_vars_alongside_connection_project_in_the_same_query() {
        std::env::set_var("DATAMK_TEST_QUERY_DATASET3", "summarydata");
        let def = cell_with_source(
            "raw_spend_hourly",
            Source::Connection {
                connection: "crm".to_string(),
                table: None,
                query: Some(
                    "SELECT * FROM \
                     `${connection.project}.${DATAMK_TEST_QUERY_DATASET3}.campaign_group_spend_by_minute`"
                        .to_string(),
                ),
                incremental: None,
            },
        );
        let r = resolve(&def, &bindings_with_crm_connection()).unwrap();
        match r.sources.get("raw_spend_hourly").unwrap() {
            ResolvedSource::Connection { target, .. } => match target {
                ConnectionTarget::Query(q) => assert_eq!(
                    q,
                    "SELECT * FROM `acme-prod-crm.summarydata.campaign_group_spend_by_minute`"
                ),
                other => panic!("expected Query target, got {other:?}"),
            },
            other => panic!("expected connection, got {other:?}"),
        }
    }

    #[test]
    fn resolve_rejects_incremental_on_a_query_source_with_adr_0007_text() {
        let def = cell_with_source(
            "raw_spend_hourly",
            Source::Connection {
                connection: "crm".to_string(),
                table: None,
                query: Some("SELECT 1".to_string()),
                incremental: Some(Incremental {
                    cursor: "updated_at".to_string(),
                    lookback: None,
                }),
            },
        );
        let err = resolve(&def, &bindings_with_crm_connection())
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "source 'raw_spend_hourly': `incremental:` is not yet supported on a `query:` \
             source. Use `table:` with `incremental:`, or drop `incremental:` — a `query:` \
             source is re-read in full each run."
        );
    }

    #[test]
    fn resolve_still_accepts_incremental_on_a_table_source() {
        // The ADR 0007 refusal must be scoped to `query:` sources only — a
        // plain `table:` + `incremental:` source (ADR 0005) is unaffected.
        let def = cell_with_source(
            "crm_accounts",
            Source::Connection {
                connection: "crm".to_string(),
                table: Some("sales.accounts".to_string()),
                query: None,
                incremental: Some(Incremental {
                    cursor: "updated_at".to_string(),
                    lookback: None,
                }),
            },
        );
        resolve(&def, &bindings_with_crm_connection()).unwrap();
    }

    #[test]
    fn resolve_rejects_a_bad_cursor_identifier() {
        let def = cell_with_source(
            "crm_accounts",
            Source::Connection {
                connection: "crm".to_string(),
                table: Some("sales.accounts".to_string()),
                query: None,
                incremental: Some(Incremental {
                    cursor: "updated at, dropped()".to_string(),
                    lookback: None,
                }),
            },
        );
        let err = resolve(&def, &bindings_with_crm_connection())
            .unwrap_err()
            .to_string();
        assert!(err.contains("crm_accounts"), "unexpected error: {err}");
        assert!(
            err.contains("updated at, dropped()"),
            "unexpected error: {err}"
        );
        assert!(
            err.contains("not a valid column identifier"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_rejects_an_unparseable_lookback() {
        let def = cell_with_source(
            "crm_accounts",
            Source::Connection {
                connection: "crm".to_string(),
                table: Some("sales.accounts".to_string()),
                query: None,
                incremental: Some(Incremental {
                    cursor: "updated_at".to_string(),
                    lookback: Some("2w".to_string()),
                }),
            },
        );
        let err = resolve(&def, &bindings_with_crm_connection())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("crm_accounts") || err.starts_with("source 'crm_accounts'"),
            "unexpected error: {err}"
        );
        assert!(
            err.contains("is not a valid duration"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_expands_s3_and_drops_empty_endpoint() {
        std::env::set_var("DATAMK_TEST_REGION", "us-west-2");
        let def = CellDef {
            cell: "c".into(),
            sources: IndexMap::new(),
            transforms: vec![],
            interface: vec![] as Vec<Export>,
            access: Default::default(),
        };
        let b = Bindings {
            catalog: Some("c".into()),
            storage: "s".into(),
            s3: Some(S3Binding {
                region: Some("${DATAMK_TEST_REGION}".to_string()),
                endpoint: Some("${DATAMK_TEST_NO_ENDPOINT:-}".to_string()),
                url_style: None,
                key_id: None,
                secret: None,
                session_token: None,
                use_ssl: Some(true),
            }),
            gcs: None,
            principals: None,
            cells: IndexMap::new(),
            connections: IndexMap::new(),
        };
        let r = resolve(&def, &b).unwrap();
        let s3 = r.s3.unwrap();
        assert_eq!(s3.region.as_deref(), Some("us-west-2"));
        assert_eq!(s3.endpoint, None); // empty expansion collapses to None
        assert_eq!(s3.use_ssl, Some(true));
    }

    #[test]
    fn resolve_expands_gcs_and_drops_empty_endpoint() {
        std::env::set_var("DATAMK_TEST_GCS_KEY", "HMACKEY");
        let def = CellDef {
            cell: "c".into(),
            sources: IndexMap::new(),
            transforms: vec![],
            interface: vec![] as Vec<Export>,
            access: Default::default(),
        };
        let b = Bindings {
            catalog: Some("c".into()),
            storage: "gs://bkt/cells/c".into(),
            s3: None,
            gcs: Some(GcsBinding {
                credentials: Some("secrets/gcs-key.json".to_string()),
                extension: None,
                key_id: Some("${DATAMK_TEST_GCS_KEY}".to_string()),
                secret: Some("shh".to_string()),
                endpoint: Some("${DATAMK_TEST_NO_ENDPOINT:-}".to_string()),
                use_ssl: None,
            }),
            principals: None,
            cells: IndexMap::new(),
            connections: IndexMap::new(),
        };
        let r = resolve(&def, &b).unwrap();
        let gcs = r.gcs.unwrap();
        assert_eq!(gcs.credentials.as_deref(), Some("secrets/gcs-key.json"));
        assert_eq!(gcs.key_id.as_deref(), Some("HMACKEY"));
        assert_eq!(gcs.secret.as_deref(), Some("shh"));
        assert_eq!(gcs.endpoint, None); // empty expansion collapses to None
    }

    #[test]
    fn is_remote_detects_object_storage_schemes() {
        assert!(is_remote("s3://b/k"));
        assert!(is_remote("gs://b/k"));
        assert!(is_remote("gcs://b/k"));
        assert!(!is_remote("/local/path"));
        assert!(!is_remote("./rel.parquet"));
        assert!(!is_remote("file:///abs"));
    }

    #[test]
    fn metadata_db_catalog_detection() {
        assert!(is_metadata_db_catalog("sqlite:cat.db"));
        assert!(is_metadata_db_catalog("postgres://u@h/db"));
        // A DuckDB-file `.ducklake` catalog is NOT concurrent-safe.
        assert!(!is_metadata_db_catalog("./cat.ducklake"));
        assert!(!is_metadata_db_catalog("/abs/cat.ducklake"));
    }

    #[test]
    fn resolved_source_remote_detection() {
        let remote = |s: &ResolvedSource| s.location().is_some_and(is_remote);
        assert!(remote(&ResolvedSource::Raw("s3://b/x".into())));
        assert!(remote(&ResolvedSource::Raw("gs://b/x".into())));
        assert!(remote(&ResolvedSource::Raw("gcs://b/x".into())));
        assert!(!remote(&ResolvedSource::Raw("./local.parquet".into())));
        assert!(remote(&ResolvedSource::Cell {
            catalog: Some("c".into()),
            storage: "s3://b/d".into(),
            table: "t".into(),
            version: None,
        }));
        assert!(!remote(&ResolvedSource::Cell {
            catalog: Some("c".into()),
            storage: "/local/d".into(),
            table: "t".into(),
            version: None,
        }));
    }
}
