use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::config::Contract;
use crate::engine;

/// The publish manifest: pins which snapshot each supported route serves.
#[derive(Debug, Serialize, Deserialize)]
pub struct Published {
    pub snapshot_id: i64,
    /// route (e.g. `orders_daily@2`) -> pinned snapshot id
    pub routes: BTreeMap<String, i64>,
}

/// The one deliberate promotion: pin the current snapshot as the supported
/// contract. `serve` then freezes supported routes at this snapshot.
pub fn run(file: &Path, profile: &str) -> Result<()> {
    let cell = engine::open(file, profile, true)?;
    let snapshot = current_snapshot(&cell.conn)?;

    let mut routes = BTreeMap::new();
    for export in &cell.def.interface {
        if export.contract == Contract::Supported {
            routes.insert(export.route()?, snapshot);
        }
    }
    if routes.is_empty() {
        tracing::warn!("no exports marked 'contract: supported'; nothing to pin");
    }

    let path = cell.dir.join(".cell").join("published.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let manifest = Published { snapshot_id: snapshot, routes };
    std::fs::write(&path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("writing {}", path.display()))?;

    tracing::info!(snapshot, path = %path.display(), "published");
    Ok(())
}

fn current_snapshot(conn: &duckdb::Connection) -> Result<i64> {
    // DuckLake exposes snapshot history via the `ducklake_snapshots(catalog)`
    // table function. Adjust if your DuckLake version renames it.
    let mut stmt = conn
        .prepare("SELECT max(snapshot_id) FROM ducklake_snapshots('lake')")
        .context("querying DuckLake snapshots")?;
    let id = stmt.query_row([], |r| r.get::<_, Option<i64>>(0))?;
    id.context("no snapshots found; run `datamk run` first")
}
