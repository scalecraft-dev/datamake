//! DuckDB-file connection config: the `type: duckdb` shape in a profile's
//! `connections:` map — a `.duckdb`/`.db` file DuckDB attaches read-only.
//! SQLMesh's default engine (and default state store) is exactly this, so
//! it is the first warehouse a discovered cell (ADR 0016) meets on a
//! laptop; it is also a perfectly good local source for any cell.
//!
//! Not to be confused with `catalog: ./x.ducklake` (datamk's own DuckLake
//! catalog) — same engine, unrelated role. This is an *upstream* you read
//! tables from.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::config::bindings::{expand, ResolvedConnection};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuckdbConnection {
    /// Path to the DuckDB database file. Relative paths resolve against the
    /// cell directory (`config::load`), like every other profile path.
    pub path: String,
}

pub fn resolve_duckdb(name: &str, c: &DuckdbConnection) -> Result<ResolvedConnection> {
    let path = expand(&c.path)?;
    if path.trim().is_empty() {
        bail!("connection '{name}' (duckdb): `path:` is required — the database file to attach");
    }
    Ok(ResolvedConnection::Duckdb { path })
}
