use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
