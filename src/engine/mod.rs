use anyhow::{Context, Result};
use duckdb::Connection;
use indexmap::IndexMap;
use std::path::{Path, PathBuf};

use crate::config::{Bindings, CellDef, ResolvedBindings, ResolvedS3, ResolvedSource};

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
    let def = CellDef::load(file)?;
    let dir = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let profile_path = dir.join("profiles").join(format!("{profile}.yaml"));
    let raw_bindings = Bindings::load(&profile_path)?;
    let bindings = crate::config::resolve(&def, &raw_bindings)?;
    let conn = Connection::open_in_memory().context("opening DuckDB")?;
    setup(&conn, &bindings, &dir, read_only)?;
    Ok(Cell {
        def,
        conn,
        dir,
        sources: bindings.sources,
        principals: bindings.principals,
    })
}

fn setup(conn: &Connection, b: &ResolvedBindings, dir: &Path, read_only: bool) -> Result<()> {
    // INSTALL fetches the extension from the registry on first run (needs network).
    conn.execute_batch("INSTALL ducklake; LOAD ducklake; INSTALL json; LOAD json;")
        .context("loading DuckLake extension")?;

    let storage = resolve_storage(&b.storage, dir)?;

    // Object storage needs httpfs; S3 also needs a secret (default: AWS credential chain).
    let uses_remote =
        is_remote(&storage) || b.sources.values().any(|s| s.is_remote()) || b.s3.is_some();
    if uses_remote {
        conn.execute_batch("INSTALL httpfs; LOAD httpfs;")
            .context("loading httpfs extension")?;
        create_s3_secret(conn, b.s3.as_ref())?;
    }

    let catalog = resolve_catalog(&b.catalog, dir)?;
    let ro = if read_only { ", READ_ONLY" } else { "" };
    conn.execute_batch(&format!(
        "ATTACH 'ducklake:{catalog}' AS lake (DATA_PATH '{storage}'{ro}); USE lake;"
    ))
    .with_context(|| format!("attaching DuckLake (catalog={catalog}, storage={storage})"))?;
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
    }
    Ok(())
}

/// Local relative source paths resolve against the cell directory (like transforms);
/// remote URIs and absolute paths pass through. Globs are preserved.
fn resolve_source_uri(uri: &str, dir: &Path) -> String {
    if is_remote(uri) {
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

fn is_remote(uri: &str) -> bool {
    uri.starts_with("s3://") || uri.starts_with("gs://") || uri.starts_with("gcs://")
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

/// Resolve the catalog binding. `sqlite:`/`postgres:` DSNs pass through; anything
/// else is treated as a local catalog file path.
fn resolve_catalog(s: &str, dir: &Path) -> Result<String> {
    if s.starts_with("sqlite:") || s.starts_with("postgres:") {
        return Ok(s.to_string());
    }
    let p = s.strip_prefix("file://").unwrap_or(s);
    resolve_local(p, dir, /* is_dir */ false)
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
    fn is_remote_detects_object_storage_schemes() {
        assert!(is_remote("s3://bucket/key"));
        assert!(is_remote("gs://bucket/key"));
        assert!(is_remote("gcs://bucket/key"));
        assert!(!is_remote("/local/path"));
        assert!(!is_remote("./relative.parquet"));
        assert!(!is_remote("file:///abs"));
    }

    #[test]
    fn esc_doubles_single_quotes() {
        assert_eq!(esc("plain"), "plain");
        assert_eq!(esc("o'brien"), "o''brien");
        assert_eq!(esc("a'b'c"), "a''b''c");
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
