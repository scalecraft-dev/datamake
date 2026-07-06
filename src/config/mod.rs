mod bindings;
pub mod deploy; // DeployConfig/Target; read only by the deploy command (Phase 3 wires it)
mod schema;

use anyhow::Result;
use std::path::{Path, PathBuf};

pub use bindings::{
    is_metadata_db_catalog, is_remote, resolve, ResolvedBindings, ResolvedConnection,
    ResolvedIncremental, ResolvedS3, ResolvedSource,
};
pub use deploy::{DeployConfig, Target};
pub use schema::{Bindings, CellDef, Contract, Export, Source, Visibility};

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
}

/// Parse `cell.yaml` + `profiles/<profile>.yaml` and resolve all `${VAR}`
/// references. No DuckDB, no filesystem writes — pure parse + env expansion.
pub fn load(file: &Path, profile: &str) -> Result<LoadedCell> {
    let def = CellDef::load(file)?;
    let dir = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let profile_path = dir.join("profiles").join(format!("{profile}.yaml"));
    let raw = Bindings::load(&profile_path)?;
    let bindings = resolve(&def, &raw)?;
    Ok(LoadedCell { def, dir, bindings })
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
}
