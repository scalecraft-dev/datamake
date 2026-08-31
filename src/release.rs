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
    // (not at config-load time — that would populate `docs[].sha256` on
    // every never-built cell) for the cell plus every discoverable export
    // that declares `docs:`, regardless of contract — the same route list
    // `context::interface`'s docs entries derive from, so identity and
    // fingerprint never disagree on what a "target" is. `load_declared`
    // also collects every definition's page (ADR 0017 §4), so
    // `docs["definition:<term>"]` fingerprints fall out of this call for
    // free.
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
        // issue #6, binding model: a bound export's declared object is never
        // in `lake` at all — pinning it to `snapshot` would record a
        // version id that governs nothing, and (downstream)
        // `serve`/`probe_exports` would apply `AT (VERSION => id)` to a
        // relation the pin can't actually describe. `serve` doesn't route
        // these regardless (`mounted_routes`), but the manifest itself
        // should not carry a pin that names no real object — `datamk
        // status`/`attach` and anything else that reads `published.json`
        // directly must not see a route implying a lake table.
        if export.is_bound() {
            tracing::info!(
                export = %export.name,
                "bound export — no snapshot to pin; datamk owns this export's contract, not its \
                 rows (see `datamk context`)"
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
        descriptions.insert(
            route.clone(),
            description_digest(
                export,
                docs_content,
                &cell.def.definitions,
                &docs_pages,
                &route,
            ),
        );
    }
    if routes.is_empty() {
        tracing::warn!("no exports marked 'contract: supported'; nothing to pin");
    }

    // ADR 0017 §5: `Published.descriptions["cell"]` — route keys always
    // carry `@major`, so no collision. Closes the gap the ADR names: the
    // cell page's fingerprint was written to `docs` but compared nowhere,
    // so an edit to it (or to the business glossary, which has no export
    // of its own to fan into) escaped the release ratchet entirely.
    let cell_page_content = docs_pages
        .iter()
        .find(|p| p.target == "cell")
        .map(|p| p.content.as_ref());
    let cell_digest = cell_description_digest(&cell.def, cell_page_content, &docs_pages);
    descriptions.insert("cell".to_string(), cell_digest.clone());

    // ADR 0012 §3 ratchet check 3: a changed description under an unchanged
    // version is a silent meaning-edit — a change in meaning is MAJOR
    // (ARCHITECTURE.md §141-143). Warned here, on the release gesture, where
    // the previous pin is at hand to compare against.
    if let Some(prev) = Published::load(&cell.dir) {
        for (route, digest) in &descriptions {
            if route == "cell" {
                continue; // no version governs the cell — handled below.
            }
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
        // ADR 0017 §5: a manifest with no prior `"cell"` entry (pre-ADR
        // 0017) draws no warning on this first release — there is nothing
        // yet to compare against. Naming the term is only ever a guess
        // ("where possible"): the digest is a single hash, so a change
        // confined to one definition can be pinpointed only when there is
        // exactly one to blame.
        if let Some(prev_cell) = prev.descriptions.get("cell") {
            if prev_cell != &cell_digest {
                let term_hint = match cell.def.definitions.as_slice() {
                    [only] => format!(" (likely the '{}' definition)", only.term),
                    _ => String::new(),
                };
                tracing::warn!(
                    cell = %cell.def.cell,
                    "the cell's meaning moved (description, cell page, or a definition edited)\
                     {term_hint} — no version governs the cell itself; review it"
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
/// and description, (ADR 0013 §9) its docs page content, and (ADR 0017 §5)
/// every definition whose `applies_to` names this route or one of its
/// columns — sorted by term, folding in each one's `term`, `aliases`,
/// `description`, and page content, in that order. Types and grain are
/// versioned by the schema itself; this digest tracks exactly the fields
/// the ADR 0012 §3 ratchet exists to guard — the ones `verify` cannot check
/// against rows. Folding in docs content and applicable definitions means a
/// prose-only edit still draws the "changed meaning without a version bump"
/// warning at the next release.
fn description_digest(
    export: &crate::config::Export,
    docs_content: Option<&str>,
    definitions: &[crate::config::Definition],
    docs_pages: &[crate::config::docs::DocsPage],
    route: &str,
) -> String {
    use crate::config::Origin;
    // ADR 0015 §6: the ratchet asks "did the AUTHOR change the meaning?" —
    // it hashes `cell.yaml`'s own words only. A description whose origin is
    // the warehouse or a modeling tool is visible to consumers through the
    // interface digest (the ETag) instead; an upstream edit must never move
    // datamk's own release gate (issue #10).
    let authored = |from: &crate::config::FromMap, field: &str| {
        from.get(field).is_none_or(|o| *o == Origin::CellYaml)
    };
    let mut input = String::new();
    if authored(&export.from, "description") {
        input.push_str(export.description.as_deref().unwrap_or(""));
    }
    for (col, spec) in &export.schema {
        input.push('\u{1f}');
        input.push_str(col);
        input.push('\u{1f}');
        if authored(&spec.from, "unit") {
            input.push_str(spec.unit.as_deref().unwrap_or(""));
        }
        input.push('\u{1f}');
        if authored(&spec.from, "description") {
            input.push_str(spec.description.as_deref().unwrap_or(""));
        }
    }
    input.push('\u{1f}');
    input.push_str(docs_content.unwrap_or(""));

    let mut applicable: Vec<&crate::config::Definition> = definitions
        .iter()
        .filter(|d| {
            d.applies_to
                .iter()
                .any(|entry| crate::context::applies_to_route(entry, route))
        })
        .collect();
    applicable.sort_by(|a, b| a.term.cmp(&b.term));
    for d in applicable {
        fold_definition(&mut input, d, docs_pages);
    }

    crate::context::sha256_hex(input.as_bytes())
}

/// The cell-wide meaning digest (ADR 0017 §5): `description`, the cell
/// page's content, and every definition in declared (canonical) order —
/// the business glossary is folded in here rather than fanned per-route,
/// since a cell-wide definition (empty `applies_to`) has no route to fan
/// into.
fn cell_description_digest(
    def: &crate::config::CellDef,
    cell_page_content: Option<&str>,
    docs_pages: &[crate::config::docs::DocsPage],
) -> String {
    let mut input = String::new();
    input.push_str(def.description.as_deref().unwrap_or(""));
    input.push('\u{1f}');
    input.push_str(cell_page_content.unwrap_or(""));
    for d in &def.definitions {
        fold_definition(&mut input, d, docs_pages);
    }
    crate::context::sha256_hex(input.as_bytes())
}

/// Fold one definition's meaning — `term`, `aliases`, `description`, page
/// content — into a digest input string, unit-separator-delimited like the
/// rest of `description_digest`'s fields.
fn fold_definition(
    input: &mut String,
    d: &crate::config::Definition,
    docs_pages: &[crate::config::docs::DocsPage],
) {
    input.push('\u{1f}');
    input.push_str(&d.term);
    input.push('\u{1f}');
    input.push_str(&d.aliases.join(","));
    input.push('\u{1f}');
    input.push_str(&d.description);
    input.push('\u{1f}');
    let target = format!("definition:{}", d.term);
    let page_content = docs_pages
        .iter()
        .find(|p| p.target == target)
        .map(|p| p.content.as_ref())
        .unwrap_or("");
    input.push_str(page_content);
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

    // issue #6, binding model: `release` must not pin a bound export — there
    // is no lake snapshot behind it to pin, and a pin naming one would let
    // `AT (VERSION => id)` be applied to a relation that isn't in the lake
    // (the exact bug class the review flagged for `serve`'s pin sites).
    #[test]
    fn release_skips_pinning_a_bound_supported_export_but_pins_its_materializing_sibling() {
        let dir = std::env::temp_dir().join(format!(
            "datamk-release-bound-{}-{}",
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
             \x20     id: bigint\n\
             \x20     val: string\n\
             \x20   contract: supported\n\
             \x20   bind: raw\n\
             sources:\n\
             \x20 raw: ./data.csv\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("sql/stg.sql"),
            "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, val)",
        )
        .unwrap();
        std::fs::write(dir.join("data.csv"), "id,val\n1,a\n2,b\n").unwrap();
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
            "a bound export must never be pinned — there is no lake snapshot behind it: \
             {manifest:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `description_digest`, with no definitions in play — the pre-ADR-0017
    /// two-argument shape, kept as a thin wrapper so the existing tests read
    /// unchanged.
    fn dd(export: &crate::config::Export, docs_content: Option<&str>) -> String {
        description_digest(export, docs_content, &[], &[], "e@1")
    }

    // ADR 0012 §3 ratchet check 3: the digest tracks exactly the meaning
    // prose — export description and per-column unit/description.
    #[test]
    fn description_digest_moves_with_meaning_and_only_meaning() {
        let base = dd(&export(Some("A row."), None), None);
        assert_eq!(base, dd(&export(Some("A row."), None), None));
        assert_ne!(base, dd(&export(Some("A different row."), None), None));
        assert_ne!(base, dd(&export(Some("A row."), Some("Gross.")), None));
    }

    // ADR 0013 §9: docs page content folds into the same digest, so editing
    // only a docs page (description and version both unchanged) still draws
    // the "changed meaning without a version bump" warning.
    #[test]
    fn description_digest_moves_with_docs_content_too() {
        let e = export(Some("A row."), None);
        let base = dd(&e, None);
        assert_eq!(base, dd(&e, Some("")));
        assert_ne!(
            base,
            dd(&e, Some("Some long-form prose.")),
            "editing docs content alone must move the digest"
        );
    }

    fn definition(
        term: &str,
        aliases: &[&str],
        description: &str,
        applies_to: &[&str],
    ) -> crate::config::Definition {
        serde_yaml::from_str(&format!(
            "term: {term}\naliases: [{}]\ndescription: {description}\napplies_to: [{}]\n",
            aliases.join(","),
            applies_to.join(","),
        ))
        .unwrap()
    }

    /// ADR 0017 §5: a definition whose `applies_to` names the route folds
    /// into that route's digest; a cell-wide (empty `applies_to`) or
    /// differently-scoped definition does not.
    #[test]
    fn description_digest_fans_in_only_definitions_naming_this_route() {
        let e = export(Some("A row."), None);
        let base = dd(&e, None);

        let applicable = [definition(
            "net_revenue",
            &[],
            "Invoiced revenue.",
            &["e@1"],
        )];
        let moved = description_digest(&e, None, &applicable, &[], "e@1");
        assert_ne!(base, moved, "an applicable definition must move the digest");

        let cell_wide = [definition("net_revenue", &[], "Invoiced revenue.", &[])];
        assert_eq!(
            base,
            description_digest(&e, None, &cell_wide, &[], "e@1"),
            "a cell-wide definition must not fan into any route's digest"
        );

        let other_route = [definition(
            "net_revenue",
            &[],
            "Invoiced revenue.",
            &["other@1"],
        )];
        assert_eq!(
            base,
            description_digest(&e, None, &other_route, &[], "e@1"),
            "a definition scoped to a different route must not move this one's digest"
        );
    }

    /// ADR 0017 §5: the cell-wide entry hashes description + cell page +
    /// every definition, so an edit to any of the three moves it.
    #[test]
    fn cell_description_digest_moves_with_description_page_and_definitions() {
        let mut def: crate::config::CellDef =
            serde_yaml::from_str("cell: c\ndescription: Daily orders.\n").unwrap();
        let base = cell_description_digest(&def, None, &[]);
        assert_eq!(base, cell_description_digest(&def, None, &[]));
        assert_ne!(
            base,
            cell_description_digest(&def, Some("Some cell page prose."), &[]),
            "a cell page edit must move it"
        );
        def.definitions = vec![definition("net_revenue", &[], "Invoiced revenue.", &[])];
        assert_ne!(
            base,
            cell_description_digest(&def, None, &[]),
            "adding a definition must move it"
        );
    }
}
