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

    // ADR 0013 §5: docs fingerprints are a release-time fact, computed here
    // (not at config-load time — that would populate `observed.docs` on
    // every never-built cell) for the cell plus every discoverable export
    // that declares `docs:`, regardless of contract — the same route list
    // `declared.docs`/`context::declared` derive from, so identity and
    // fingerprint never disagree on what a "target" is.
    let doc_routes = crate::context::discoverable_routes(&cell.def)?;
    let docs_pages = crate::config::docs::load_declared(&cell.dir, &cell.def, &doc_routes)?;
    let mut docs = BTreeMap::new();
    for page in &docs_pages {
        docs.insert(
            page.target.clone(),
            crate::context::DocsFingerprint {
                sha256: page.sha256.clone(),
                bytes: page.bytes,
            },
        );
    }

    let mut routes = BTreeMap::new();
    let mut versions = BTreeMap::new();
    let mut descriptions = BTreeMap::new();
    for export in &cell.def.interface {
        if export.contract == Contract::Supported {
            let route = export.route()?;
            routes.insert(route.clone(), snapshot);
            versions.insert(route.clone(), export.version.clone());
            let docs_content = docs_pages
                .iter()
                .find(|p| p.target == route)
                .map(|p| p.content.as_ref());
            descriptions.insert(route, description_digest(export, docs_content));
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
        docs,
    };
    std::fs::write(&path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("writing {}", path.display()))?;

    tracing::info!(snapshot, path = %path.display(), "released");
    Ok(())
}

/// Digest of an export's meaning prose: its description, every column's unit
/// and description, and (ADR 0013 §9) its docs page content, in declared
/// order. Types and grain are versioned by the schema itself; this digest
/// tracks exactly the fields the ADR 0012 §3 ratchet exists to guard — the
/// ones `verify` cannot check against rows. Folding in docs content means a
/// prose-only edit to a page still draws the "changed meaning without a
/// version bump" warning at the next release — setting both `description`
/// and `docs:` is correct, not an error, and both are meaning.
fn description_digest(export: &crate::config::Export, docs_content: Option<&str>) -> String {
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
    input.push('\u{1f}');
    input.push_str(docs_content.unwrap_or(""));
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
        let base = description_digest(&export(Some("A row."), None), None);
        assert_eq!(
            base,
            description_digest(&export(Some("A row."), None), None)
        );
        assert_ne!(
            base,
            description_digest(&export(Some("A different row."), None), None)
        );
        assert_ne!(
            base,
            description_digest(&export(Some("A row."), Some("Gross.")), None)
        );
    }

    // ADR 0013 §9: docs page content folds into the same digest, so editing
    // only a docs page (description and version both unchanged) still draws
    // the "changed meaning without a version bump" warning.
    #[test]
    fn description_digest_moves_with_docs_content_too() {
        let e = export(Some("A row."), None);
        let base = description_digest(&e, None);
        assert_eq!(base, description_digest(&e, Some("")));
        assert_ne!(
            base,
            description_digest(&e, Some("Some long-form prose.")),
            "editing docs content alone must move the digest"
        );
    }
}
