use super::schema::{parse_duration, Bindings, CellDef, Connection, Incremental, Source};
use anyhow::{bail, Result};
use indexmap::IndexMap;

/// A cell's environment config with all `${VAR}` references expanded.
#[derive(Debug, Clone)]
pub struct ResolvedBindings {
    /// `Some` ⇒ direct attach; `None` ⇒ published-artifact mode (ADR 0004 §11).
    pub catalog: Option<String>,
    pub storage: String,
    pub s3: Option<ResolvedS3>,
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
    /// A warehouse table with its connection config inlined (as `Cell` inlines
    /// its location). `connection` keeps the reference name for the engine's
    /// attach alias, shared by every source on the same connection.
    Connection {
        connection: String,
        config: ResolvedConnection,
        table: String,
        /// Watermarked-read config (ADR 0005), resolved: cursor expanded and
        /// identifier-validated, lookback parsed to a `Duration`. Consumed by
        /// the engine's incremental bind path (ADR 0005 Stage 2).
        incremental: Option<ResolvedIncremental>,
    },
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
    },
}

impl ResolvedSource {
    /// Whether this source reads from object storage (needs httpfs/S3 secret).
    /// Connection sources read through a scanner extension, not httpfs.
    pub fn is_remote(&self) -> bool {
        let loc = match self {
            ResolvedSource::Raw(uri) => uri.as_str(),
            ResolvedSource::Cell { storage, .. } => storage.as_str(),
            ResolvedSource::Connection { .. } => return false,
        };
        is_remote(loc)
    }
}

/// Whether a URI points at object storage (`s3://`/`gs://`/`gcs://`), and so
/// needs the httpfs extension + an S3 secret. The inverse — a local/file path —
/// is what deploy pre-flight refuses for a remote workload.
pub fn is_remote(uri: &str) -> bool {
    uri.starts_with("s3://") || uri.starts_with("gs://") || uri.starts_with("gcs://")
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
            use_ssl: s.use_ssl,
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
                incremental,
            } => {
                let conn = b.connections.get(connection).ok_or_else(|| {
                    anyhow::anyhow!(
                        "source '{name}' uses connection '{connection}', but the profile has no \
                         `connections.{connection}` entry"
                    )
                })?;
                let incremental = incremental
                    .as_ref()
                    .map(|inc| resolve_incremental(name, inc))
                    .transpose()?;
                ResolvedSource::Connection {
                    connection: connection.clone(),
                    config: resolve_connection(conn)?,
                    table: expand(table)?,
                    incremental,
                }
            }
        };
        sources.insert(name.clone(), resolved);
    }

    let principals = expand_opt(&b.principals)?;

    Ok(ResolvedBindings {
        catalog: expand_opt(&b.catalog)?,
        storage: expand(&b.storage)?,
        s3,
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
    if !is_valid_cursor_identifier(&cursor) {
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

/// `[A-Za-z_][A-Za-z0-9_]*` — a bare column identifier, no dots/quotes/spaces.
fn is_valid_cursor_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn resolve_connection(c: &Connection) -> Result<ResolvedConnection> {
    Ok(match c {
        Connection::Bigquery(bq) => ResolvedConnection::Bigquery {
            project: expand(&bq.project)?,
            billing_project: expand_opt(&bq.billing_project)?,
            credentials: expand_opt(&bq.credentials)?,
        },
    })
}

fn expand_opt(o: &Option<String>) -> Result<Option<String>> {
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
    use super::super::schema::{BigQueryConnection, Bindings, CellLocation, Export, S3Binding};
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
                table: "sales.accounts".to_string(),
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
            }),
        );
        let b = Bindings {
            catalog: Some("c".into()),
            storage: "s".into(),
            s3: None,
            principals: None,
            cells: IndexMap::new(),
            connections,
        };
        let r = resolve(&def, &b).unwrap();
        let src = r.sources.get("crm_accounts").unwrap();
        // A connection source reads through a scanner extension, not httpfs.
        assert!(!src.is_remote());
        match src {
            ResolvedSource::Connection {
                connection,
                config: ResolvedConnection::Bigquery { project, .. },
                table,
                incremental,
            } => {
                assert_eq!(connection, "crm");
                assert_eq!(project, "acme-prod-crm");
                assert_eq!(table, "sales.accounts");
                assert!(incremental.is_none());
            }
            other => panic!("expected connection, got {other:?}"),
        }
    }

    #[test]
    fn resolve_errors_when_connection_is_missing() {
        let def = cell_with_source(
            "crm_accounts",
            Source::Connection {
                connection: "crm".to_string(),
                table: "sales.accounts".to_string(),
                incremental: None,
            },
        );
        let b = Bindings {
            catalog: Some("c".into()),
            storage: "s".into(),
            s3: None,
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
            }),
        );
        Bindings {
            catalog: Some("c".into()),
            storage: "s".into(),
            s3: None,
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
                table: "sales.accounts".to_string(),
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
                table: "sales.accounts".to_string(),
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

    #[test]
    fn resolve_rejects_a_bad_cursor_identifier() {
        let def = cell_with_source(
            "crm_accounts",
            Source::Connection {
                connection: "crm".to_string(),
                table: "sales.accounts".to_string(),
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
                table: "sales.accounts".to_string(),
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
                use_ssl: Some(true),
            }),
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
        assert!(ResolvedSource::Raw("s3://b/x".into()).is_remote());
        assert!(ResolvedSource::Raw("gs://b/x".into()).is_remote());
        assert!(ResolvedSource::Raw("gcs://b/x".into()).is_remote());
        assert!(!ResolvedSource::Raw("./local.parquet".into()).is_remote());
        assert!(ResolvedSource::Cell {
            catalog: Some("c".into()),
            storage: "s3://b/d".into(),
            table: "t".into(),
            version: None,
        }
        .is_remote());
        assert!(!ResolvedSource::Cell {
            catalog: Some("c".into()),
            storage: "/local/d".into(),
            table: "t".into(),
            version: None,
        }
        .is_remote());
    }
}
