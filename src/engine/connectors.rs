//! Warehouse connectors (ADR 0003). A connector is three answers — which
//! extension(s), the ATTACH statement, and how to validate/quote a table path —
//! realized as match arms on `ResolvedConnection`. Connectors add DuckDB
//! extensions, not Rust dependencies, so there is no cargo feature to gate.

use anyhow::{bail, Result};
use indexmap::IndexMap;
use std::path::Path;

use crate::config::{ResolvedConnection, ResolvedSource};

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
            ResolvedConnection::Bigquery { .. } => {
                "INSTALL bigquery FROM community; LOAD bigquery;"
            }
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
            } => {
                let mut cs = format!("project={project}");
                if let Some(bp) = billing_project {
                    cs.push_str(&format!(" billing_project={bp}"));
                }
                format!(
                    "ATTACH IF NOT EXISTS '{}' AS \"{}\" (TYPE bigquery, READ_ONLY);",
                    super::esc(&cs),
                    quote(alias)
                )
            }
        }
    }

    /// Validate + quote the connector-scoped table path against the connector's
    /// expected shape, resolving it under the attach alias.
    pub fn qualify(&self, alias: &str, table: &str) -> Result<String> {
        match self {
            ResolvedConnection::Bigquery { .. } => {
                match table.split('.').collect::<Vec<_>>().as_slice() {
                    [dataset, tbl] if !dataset.is_empty() && !tbl.is_empty() => Ok(format!(
                        "\"{}\".\"{}\".\"{}\"",
                        quote(alias),
                        quote(dataset),
                        quote(tbl)
                    )),
                    _ => bail!(
                        "bigquery source table must be `dataset.table`, got '{table}' \
                         (the project comes from the connection; a cross-project read \
                         is a second connection, not a three-part name)"
                    ),
                }
            }
        }
    }

    fn credentials(&self) -> Option<&str> {
        match self {
            ResolvedConnection::Bigquery { credentials, .. } => credentials.as_deref(),
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
    let mut want: Option<(&str, String)> = None;
    for (name, src) in sources {
        let ResolvedSource::Connection { config, .. } = src else {
            continue;
        };
        let Some(path) = config.credentials() else {
            continue;
        };
        let resolved = resolve_credentials_path(path, dir);
        match &want {
            Some((first, existing)) if *existing != resolved => bail!(
                "sources '{first}' and '{name}' use connections with different credentials \
                 files ('{existing}' vs '{resolved}'); one run supports one ADC key file"
            ),
            Some(_) => {}
            None => want = Some((name, resolved)),
        }
    }
    if let Some((source, path)) = want {
        if !Path::new(&path).is_file() {
            bail!(
                "credentials file '{path}' (connection used by source '{source}') \
                 does not exist or is not a file"
            );
        }
        std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", &path);
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
        }
    }

    #[test]
    fn bigquery_attach_sql_is_read_only_and_aliased() {
        assert_eq!(
            bq("acme-prod", None, None).attach_sql("__conn_crm"),
            "ATTACH IF NOT EXISTS 'project=acme-prod' AS \"__conn_crm\" \
             (TYPE bigquery, READ_ONLY);"
        );
    }

    #[test]
    fn bigquery_attach_sql_includes_billing_project_when_set() {
        assert_eq!(
            bq("acme-prod", Some("acme-billing"), None).attach_sql("__conn_crm"),
            "ATTACH IF NOT EXISTS 'project=acme-prod billing_project=acme-billing' \
             AS \"__conn_crm\" (TYPE bigquery, READ_ONLY);"
        );
    }

    #[test]
    fn bigquery_qualify_accepts_dataset_table() {
        assert_eq!(
            bq("p", None, None)
                .qualify("__conn_crm", "sales.accounts")
                .unwrap(),
            "\"__conn_crm\".\"sales\".\"accounts\""
        );
    }

    #[test]
    fn bigquery_qualify_quotes_identifiers() {
        assert_eq!(
            bq("p", None, None)
                .qualify("__conn_crm", "sa\"les.acc\"ounts")
                .unwrap(),
            "\"__conn_crm\".\"sa\"\"les\".\"acc\"\"ounts\""
        );
    }

    #[test]
    fn bigquery_qualify_rejects_one_and_three_part_names() {
        for bad in ["accounts", "proj.sales.accounts", "sales.", ".accounts", ""] {
            let err = bq("p", None, None)
                .qualify("__conn_crm", bad)
                .unwrap_err()
                .to_string();
            assert!(err.contains("dataset.table"), "for '{bad}': {err}");
        }
    }

    fn conn_source(config: ResolvedConnection) -> ResolvedSource {
        ResolvedSource::Connection {
            connection: "crm".to_string(),
            config,
            table: "sales.accounts".to_string(),
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
}
