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
