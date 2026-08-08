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

    // issue #6: a `materialize: never` table is never in `lake` at all —
    // pinning it to `snapshot` would record a version id that governs
    // nothing, and (downstream) `serve`/`probe_exports` would apply
    // `AT (VERSION => id)` to a relation the pin can't actually describe.
    // `serve` doesn't route these regardless (`mounted_routes`), but the
    // manifest itself should not carry a pin that names no real object —
    // `datamk status`/`attach` and anything else that reads
    // `published.json` directly must not see a route implying a lake table.
    let never_tables = crate::config::never_backed_tables(&cell.transforms);

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
        if export.contract != Contract::Supported {
            continue;
        }
        if never_tables.contains(export.source_object()) {
            tracing::info!(
                export = %export.name,
                "materialize: never — no snapshot to pin; datamk owns this export's contract, \
                 not its rows (see `datamk context`)"
            );
            continue;
        }
        let route = export.route()?;
        routes.insert(route.clone(), snapshot);
        versions.insert(route.clone(), export.version.clone());
        let docs_content = docs_pages
            .iter()
            .find(|p| p.target == route)
            .map(|p| p.content.as_ref());
        descriptions.insert(route, description_digest(export, docs_content));
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

    // issue #6: `release` must not pin a `materialize: never` export — there
    // is no lake snapshot behind it to pin, and a pin naming one would let
    // `AT (VERSION => id)` be applied to a relation that isn't in the lake
    // (the exact bug class the review flagged for `serve`'s pin sites).
    #[test]
    fn release_skips_pinning_a_never_backed_supported_export_but_pins_its_materializing_sibling() {
        let dir = std::env::temp_dir().join(format!(
            "datamk-release-never-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sql")).unwrap();
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::write(
            dir.join("cell.yaml"),
            "cell: mixed_release\n\
             transforms:\n\
             \x20 - sql/stg.sql\n\
             \x20 - sql: sql/virtual_pii.sql\n\
             \x20   materialize: never\n\
             interface:\n\
             \x20 - name: stg\n\
             \x20   version: 1.0.0\n\
             \x20   description: A materialized, pinnable export.\n\
             \x20   grain: [id]\n\
             \x20   schema:\n\
             \x20     id: integer\n\
             \x20     val: string\n\
             \x20   contract: supported\n\
             \x20 - name: virtual_pii\n\
             \x20   version: 1.0.0\n\
             \x20   description: PII rows datamk verifies but never stores.\n\
             \x20   grain: [id]\n\
             \x20   schema:\n\
             \x20     id: integer\n\
             \x20     val: string\n\
             \x20   contract: supported\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("sql/stg.sql"),
            "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, val)",
        )
        .unwrap();
        std::fs::write(dir.join("sql/virtual_pii.sql"), "SELECT * FROM stg").unwrap();
        std::fs::write(
            dir.join("profiles/local.yaml"),
            "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
        )
        .unwrap();

        crate::engine::run(
            &dir.join("cell.yaml"),
            "local",
            None,
            crate::engine::RunOptions::default(),
        )
        .expect("build the mixed cell");

        run(&dir.join("cell.yaml"), "local").expect("release the mixed cell");

        let manifest = Published::load(&dir).expect("published.json must have been written");
        assert!(
            manifest.routes.contains_key("stg@1"),
            "the materializing export must be pinned: {manifest:?}"
        );
        assert!(
            !manifest.routes.contains_key("virtual_pii@1"),
            "a never-backed export must never be pinned — there is no lake snapshot behind it: \
             {manifest:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
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
