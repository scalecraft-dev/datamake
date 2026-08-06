use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// The release manifest: pins which snapshot each supported route serves.
///
/// Written by `release` (`.cell/published.json`) and read by `serve` to freeze
/// supported routes at a fixed snapshot. Lives in a neutral module so `serve`
/// does not import from a command module.
#[derive(Debug, Serialize, Deserialize)]
pub struct Published {
    pub snapshot_id: i64,
    /// route (e.g. `orders_daily@2`) -> pinned snapshot id
    pub routes: BTreeMap<String, i64>,
    /// route -> full semver at release time (ADR 0012 §3 ratchet check 3):
    /// what lets the next `release` see that meaning changed while the
    /// version didn't. Defaults keep pre-ADR-0012 manifests parsing.
    #[serde(default)]
    pub versions: BTreeMap<String, String>,
    /// route -> digest of the export's meaning prose (description + per-column
    /// unit/description). A changed digest without a version bump is at
    /// minimum a warning — a change in meaning is MAJOR, and silent
    /// meaning-edits are the exact betrayal ARCHITECTURE.md §141-143 names.
    #[serde(default)]
    pub descriptions: BTreeMap<String, String>,
}

impl Published {
    /// Read the manifest from a cell directory, if present and well-formed.
    /// The deploy artifact bundle ships this file into the pods, so the
    /// Builder's compaction (ADR 0004 §10) and `rollback`'s pin guard read the
    /// same pins locally and in-cluster.
    pub fn load(dir: &Path) -> Option<Published> {
        let raw = std::fs::read_to_string(dir.join(".cell").join("published.json")).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Every pinned snapshot id (deduplicated).
    pub fn pinned_snapshots(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self.routes.values().copied().collect();
        ids.push(self.snapshot_id);
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}
