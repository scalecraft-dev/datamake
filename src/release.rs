use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

use crate::config::Contract;
use crate::engine;
use crate::manifest::Published;

/// Pin the current snapshot as the supported contract. `release` reads exports
/// already marked `contract: supported` and freezes them at the current snapshot
/// (`.cell/published.json`); `serve` then serves supported routes from this pin.
/// Promotion to `supported` is a separate, reviewed `cell.yaml` edit — not this
/// command.
pub fn run(file: &Path, profile: &str) -> Result<()> {
    let cell = engine::open(file, profile, true)?;
    let snapshot = current_snapshot(&cell.conn)?;

    let mut routes = BTreeMap::new();
    let mut versions = BTreeMap::new();
    let mut descriptions = BTreeMap::new();
    for export in &cell.def.interface {
        if export.contract == Contract::Supported {
            let route = export.route()?;
            routes.insert(route.clone(), snapshot);
            versions.insert(route.clone(), export.version.clone());
            descriptions.insert(route, description_digest(export));
        }
    }
    if routes.is_empty() {
        tracing::warn!("no exports marked 'contract: supported'; nothing to pin");
    }

    // ADR 0012 §3 ratchet check 3: a changed description under an unchanged
    // version is a silent meaning-edit — a change in meaning is MAJOR
    // (ARCHITECTURE.md §141-143). Warned here, on the release gesture, where
    // the previous pin is at hand to compare against.
    if let Some(prev) = Published::load(&cell.dir) {
        for (route, digest) in &descriptions {
            let same_version = prev.versions.get(route) == versions.get(route);
            let description_changed = prev
                .descriptions
                .get(route)
                .is_some_and(|old| old != digest);
            if same_version && description_changed {
                tracing::warn!(
                    route = %route,
                    "description changed without a version bump — a change in meaning is a \
                     MAJOR change (ARCHITECTURE.md); bump the export version so consumers \
                     see the contract moved"
                );
            }
        }
    }

    let path = cell.dir.join(".cell").join("published.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let manifest = Published {
        snapshot_id: snapshot,
        routes,
        versions,
        descriptions,
    };
    std::fs::write(&path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("writing {}", path.display()))?;

    tracing::info!(snapshot, path = %path.display(), "released");
    Ok(())
}

/// Digest of an export's meaning prose: its description plus every column's
/// unit and description, in declared order. Types and grain are versioned by
/// the schema itself; this digest tracks exactly the fields the ADR 0012 §3
/// ratchet exists to guard — the ones `verify` cannot check against rows.
fn description_digest(export: &crate::config::Export) -> String {
    let mut input = String::new();
    input.push_str(export.description.as_deref().unwrap_or(""));
    for (col, spec) in &export.schema {
        input.push('\u{1f}');
        input.push_str(col);
        input.push('\u{1f}');
        input.push_str(spec.unit.as_deref().unwrap_or(""));
        input.push('\u{1f}');
        input.push_str(spec.description.as_deref().unwrap_or(""));
    }
    crate::context::sha256_hex(input.as_bytes())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CellDef;

    fn export(desc: Option<&str>, col_desc: Option<&str>) -> crate::config::Export {
        let yaml = format!(
            "cell: c\ninterface:\n  - name: e\n    version: 1.0.0\n{}    schema:\n      revenue:\n        type: decimal\n{}",
            desc.map(|d| format!("    description: {d}\n")).unwrap_or_default(),
            col_desc
                .map(|d| format!("        description: {d}\n"))
                .unwrap_or_default(),
        );
        serde_yaml::from_str::<CellDef>(&yaml)
            .unwrap()
            .interface
            .remove(0)
    }

    // ADR 0012 §3 ratchet check 3: the digest tracks exactly the meaning
    // prose — export description and per-column unit/description.
    #[test]
    fn description_digest_moves_with_meaning_and_only_meaning() {
        let base = description_digest(&export(Some("A row."), None));
        assert_eq!(base, description_digest(&export(Some("A row."), None)));
        assert_ne!(
            base,
            description_digest(&export(Some("A different row."), None))
        );
        assert_ne!(
            base,
            description_digest(&export(Some("A row."), Some("Gross.")))
        );
    }
}
