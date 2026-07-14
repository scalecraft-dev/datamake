mod bindings;
mod connections; // per-connector config shapes (`connections::bigquery`, …)
pub mod deploy; // DeployConfig/Target; read only by the deploy command (Phase 3 wires it)
mod schema;

use anyhow::Result;
use std::path::{Path, PathBuf};

pub use bindings::{
    is_gcs, is_metadata_db_catalog, is_remote, is_s3, resolve, ConnectionTarget, ResolvedBindings,
    ResolvedConnection, ResolvedGcs, ResolvedIncremental, ResolvedS3, ResolvedSource,
};
pub use deploy::{DeployConfig, Target};
pub use schema::{
    resolve_transforms, Bindings, CellDef, Contract, Export, MaterializeStrategy,
    ResolvedTransform, Source, Visibility,
};

/// A cell parsed and resolved against a profile — **without a database
/// connection**. This is the pure prefix of `engine::open`: `deploy` inspects a
/// cell through `load` so it never incurs the DuckDB side effect of attaching a
/// lake. `engine::open` builds on it by adding the connection.
pub struct LoadedCell {
    pub def: CellDef,
    /// Directory containing the cell definition; transforms, profiles, and the
    /// deploy overlay resolve relative to it.
    pub dir: PathBuf,
    pub bindings: ResolvedBindings,
    /// `def.transforms`, validated and normalized (ADR 0008 work item 1):
    /// declarative table names resolved (stem or `table:` override),
    /// collision- and identifier-checked. The engine's run loop and
    /// `verify_replay` dispatch on this, never on `def.transforms` directly.
    pub transforms: Vec<ResolvedTransform>,
}

/// The cell directory a `-f`/`--file` path implies: its parent, or `.` when
/// `file` has no parent (a bare filename like the default `cell.yaml`).
/// Shared by `load()` (transforms/profiles resolve against it) and the
/// CLI's log-file placement (`.cell/logs` defaults here too, so a log file
/// and the catalog/data it narrates live under the same cell by default).
pub fn cell_dir(file: &Path) -> PathBuf {
    file.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// Parse `cell.yaml` + `profiles/<profile>.yaml` and resolve all `${VAR}`
/// references. No DuckDB, no filesystem writes — pure parse + env expansion.
pub fn load(file: &Path, profile: &str) -> Result<LoadedCell> {
    let mut def = CellDef::load(file)?;
    let dir = cell_dir(file);
    let profile_path = dir.join("profiles").join(format!("{profile}.yaml"));
    let raw = Bindings::load(&profile_path)?;
    let mut bindings = resolve(&def, &raw)?;
    // Relative `gcs.credentials`/`gcs.extension` paths resolve against the
    // cell directory, like transforms and a connection's `credentials`
    // (engine::connectors).
    if let Some(g) = bindings.gcs.as_mut() {
        for p in [&mut g.credentials, &mut g.extension].into_iter().flatten() {
            if !Path::new(p.as_str()).is_absolute() {
                *p = dir.join(p.as_str()).to_string_lossy().into_owned();
            }
        }
    }

    // ADR 0008: table naming, key shape, and cross-entry collision — pure,
    // offline, no DB. Every caller of `load` (the engine, `deploy`'s
    // artifact/preflight inspection) gets a validated cell or an error
    // before anything opens a connection.
    let transforms = resolve_transforms(&def.transforms)?;

    // ADR 0008 guard 4c: a `replace` model that references an incremental
    // source's name by word-boundary token is a resolve-time hard error —
    // before anything touches a warehouse. Needs `dir` to read each
    // `replace` model's file text (metadata matching against engine-owned
    // names, never SQL parsing). Belongs at this seam for the same reason
    // grain inheritance does, below.
    crate::verify::check_replace_incremental_gate(&def, &dir, &transforms)?;

    // ADR 0008 Consequences: an export whose `source` is a `materialize:`
    // target inherits `grain:` from that entry's `key:` when `grain:` is
    // omitted, and an explicit `grain:` there must contain the key. Applied
    // here — the one place every consumer of `def.interface` (verify, serve,
    // openapi) shares — so `export.grain` is already the effective value
    // everywhere it's read; no consumer needs to know `materialize:` exists.
    crate::verify::apply_declarative_grain_inheritance(&mut def, &transforms)?;

    Ok(LoadedCell {
        def,
        dir,
        bindings,
        transforms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_resolves_a_cell_without_a_db() {
        let loaded = load(Path::new("test/integrations/orders/cell.yaml"), "local").unwrap();
        assert_eq!(loaded.def.cell, "orders");
        // local profile -> direct-attach mode: file-backed catalog, local storage.
        let catalog = loaded.bindings.catalog.as_deref().unwrap();
        assert_eq!(catalog, "./.cell/catalog.ducklake");
        assert!(!is_metadata_db_catalog(catalog));
        assert!(!is_remote(&loaded.bindings.storage));
    }

    #[test]
    fn load_resolves_a_deployable_prod_profile_offline() {
        // prod.yaml uses ${VAR:-default}, so it resolves with no env set.
        let loaded = load(Path::new("test/integrations/orders/cell.yaml"), "prod").unwrap();
        // Deployable shape (ADR 0004): NO catalog — published-artifact mode.
        assert!(loaded.bindings.catalog.is_none());
        assert!(is_remote(&loaded.bindings.storage));
    }

    // ADR 0008 work item 1/4, exercised end to end through the real seam:
    // `load()` is the single chokepoint every caller (`engine::open`,
    // `deploy`) shares, so this is the test that actually proves grain
    // inheritance reaches `serve`/`openapi`, not just the unit-level
    // `verify::apply_declarative_grain_inheritance` in isolation.
    #[test]
    fn load_resolves_declarative_transforms_and_inherits_grain_end_to_end() {
        let dir = std::env::temp_dir().join(format!(
            "datamk-config-load-mat-{}-{}",
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
            "cell: t\n\
             transforms:\n\
             \x20 - sql: sql/fct_flights.sql\n\
             \x20   materialize: upsert\n\
             \x20   key: [flight_id]\n\
             interface:\n\
             \x20 - name: fct_flights\n\
             \x20   version: 1.0.0\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("sql/fct_flights.sql"),
            "SELECT * FROM (VALUES (1, 'AA')) AS t(flight_id, carrier)",
        )
        .unwrap();
        std::fs::write(
            dir.join("profiles/local.yaml"),
            "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
        )
        .unwrap();

        let loaded = load(&dir.join("cell.yaml"), "local").unwrap();
        assert_eq!(loaded.transforms.len(), 1);
        assert_eq!(loaded.transforms[0].table, "fct_flights");
        assert_eq!(loaded.transforms[0].key, vec!["flight_id".to_string()]);
        // The export declared no `grain:` — it must have inherited `key:`.
        assert_eq!(loaded.def.interface[0].grain, vec!["flight_id".to_string()]);
    }
}
