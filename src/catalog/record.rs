//! The sidecar `datamk sync` writes and every credential-light consumer
//! reads (ADR 0016 §5): `.cell/deployed_catalog.json`, on the
//! `SourceDescriptionsRecord` pattern — written by a build-side process
//! with state and warehouse credentials, read by `context`, `serve`,
//! `verify`, `release` with none, shipped inside the deploy artifact.
//!
//! Freshness is `(cell_yaml_digest, profile)`, like its siblings — and
//! nothing else, on purpose: a discovered cell's record is refreshed by the
//! deploy that follows a `sqlmesh plan` (ADR 0016 §5), so it is exactly as
//! current as the last deploy by construction, and `discovered_from.
//! {plan_id, synced_at}` in the document say which plan a reader is
//! looking at. A clock on top of that could only produce false alarms.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::ir::DeployedCatalog;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployedCatalogRecord {
    pub written_at: String,
    pub datamk_version: String,
    pub cell_yaml_digest: String,
    pub profile: String,
    /// `schema.table` -> the authored version of each `contract: supported`
    /// override at sync time (ADR 0016 §6): what lets the next sync detect
    /// an upstream data change under an unchanged supported version.
    #[serde(default)]
    pub pins: indexmap::IndexMap<String, String>,
    pub catalog: DeployedCatalog,
}

/// Why a record was not used — each reason is named in the consumer's
/// error/note, never collapsed into "missing".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staleness {
    Missing,
    Unreadable(String),
    CellYamlChanged,
    Profile { record: String, wanted: String },
}

impl std::fmt::Display for Staleness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Staleness::Missing => write!(f, "no .cell/deployed_catalog.json — run `datamk sync`"),
            Staleness::Unreadable(e) => write!(
                f,
                ".cell/deployed_catalog.json is unreadable ({e}) — re-run `datamk sync`"
            ),
            Staleness::CellYamlChanged => write!(
                f,
                ".cell/deployed_catalog.json was synced from a different cell.yaml — re-run \
                 `datamk sync`"
            ),
            Staleness::Profile { record, wanted } => write!(
                f,
                ".cell/deployed_catalog.json was synced under profile '{record}', not \
                 '{wanted}' — run `datamk sync -p {wanted}`"
            ),
        }
    }
}

impl DeployedCatalogRecord {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(".cell").join("deployed_catalog.json")
    }

    pub fn load(dir: &Path) -> std::result::Result<Self, Staleness> {
        let path = Self::path(dir);
        if !path.exists() {
            return Err(Staleness::Missing);
        }
        let raw =
            std::fs::read_to_string(&path).map_err(|e| Staleness::Unreadable(e.to_string()))?;
        serde_json::from_str(&raw).map_err(|e| Staleness::Unreadable(e.to_string()))
    }

    /// `load`, gated: digest match and profile match. The one place
    /// freshness is decided, so `context`, `serve` and `sync` can never
    /// apply different rules to the same file.
    pub fn fresh_for(
        dir: &Path,
        cell_yaml_digest: &str,
        profile: &str,
    ) -> std::result::Result<Self, Staleness> {
        let r = Self::load(dir)?;
        if r.cell_yaml_digest != cell_yaml_digest {
            return Err(Staleness::CellYamlChanged);
        }
        if r.profile != profile {
            return Err(Staleness::Profile {
                record: r.profile.clone(),
                wanted: profile.to_string(),
            });
        }
        Ok(r)
    }

    pub fn write(&self, dir: &Path) -> Result<PathBuf> {
        let path = Self::path(dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }
}
