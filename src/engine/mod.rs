mod connectors;

use anyhow::{Context, Result};
use duckdb::Connection;
use indexmap::IndexMap;
use std::path::{Path, PathBuf};

use crate::config::{CellDef, ResolvedBindings, ResolvedS3, ResolvedSource};

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
}

pub fn open(file: &Path, profile: &str, read_only: bool) -> Result<Cell> {
    // The pure parse+resolve prefix lives in `config::load` (no DB); `open` is
    // that plus a connection. `deploy` uses `config::load` directly to inspect a
    // cell without ever opening a database.
    let loaded = crate::config::load(file, profile)?;
    let conn = Connection::open_in_memory().context("opening DuckDB")?;
    setup(&conn, &loaded.bindings, &loaded.dir, read_only)?;
    Ok(Cell {
        def: loaded.def,
        conn,
        dir: loaded.dir,
        sources: loaded.bindings.sources,
        principals: loaded.bindings.principals,
    })
}

fn setup(conn: &Connection, b: &ResolvedBindings, dir: &Path, read_only: bool) -> Result<()> {
    // INSTALL fetches the extension from the registry on first run (needs network).
    conn.execute_batch("INSTALL ducklake; LOAD ducklake; INSTALL json; LOAD json;")
        .context("loading DuckLake extension")?;

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

    let catalog = resolve_catalog(&b.catalog, dir)?;
    load_catalog_extension(conn, &catalog)?;
    let ro = if read_only { ", READ_ONLY" } else { "" };
    conn.execute_batch(&format!(
        "ATTACH 'ducklake:{catalog}' AS lake (DATA_PATH '{storage}'{ro}); USE lake;"
    ))
    .with_context(|| format!("attaching DuckLake (catalog={catalog}, storage={storage})"))?;
    Ok(())
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
pub fn run(file: &Path, profile: &str) -> Result<()> {
    let cell = open(file, profile, false)?;
    tracing::info!(cell = %cell.def.cell, profile, "running pipeline");

    // Sources are session-local TEMP VIEWs: visible to transforms, never committed
    // to the catalog.
    connectors::prepare(&cell.sources, &cell.dir)?;
    for (i, (name, src)) in cell.sources.iter().enumerate() {
        bind_source(&cell.conn, i, name, src, &cell.dir)?;
    }

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
    cell.conn
        .execute_batch("COMMIT")
        .context("commit snapshot")?;

    tracing::info!("verifying interface");
    crate::verify::check(&cell.conn, &cell.def)?;

    tracing::info!("pipeline complete");
    Ok(())
}

/// Bind one source as a TEMP VIEW. Raw sources read a path directly; cell sources
/// attach another cell's DuckLake read-only and read its table by name (optionally
/// pinned to a snapshot) — composing through the catalog, not raw files.
fn bind_source(
    conn: &Connection,
    idx: usize,
    name: &str,
    src: &ResolvedSource,
    dir: &Path,
) -> Result<()> {
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
            let catalog = resolve_catalog(catalog, dir)?;
            let storage = resolve_storage(storage, dir)?;
            // OVERRIDE_DATA_PATH: trust the storage we were handed rather than the
            // absolute path A happened to record at build time (host/representation
            // differences shouldn't break a reference).
            conn.execute_batch(&format!(
                "ATTACH IF NOT EXISTS 'ducklake:{}' AS {alias} \
                 (DATA_PATH '{}', READ_ONLY, OVERRIDE_DATA_PATH true);",
                esc(&catalog),
                esc(&storage)
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
            conn.execute_batch(&format!(
                "CREATE OR REPLACE TEMP VIEW \"{view}\" AS SELECT * FROM {qualified};"
            ))
            .with_context(|| format!("binding connection source '{name}' -> {table}"))?;
            tracing::info!(source = %name, connection = %connection, table = %table, "bound connection source");
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
}
