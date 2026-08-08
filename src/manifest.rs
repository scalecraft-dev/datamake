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
    /// target (`"cell"` or route key) -> docs page fingerprint (ADR 0013 §5):
    /// computed at release time from the same pages `declared.docs` names,
    /// carried into `observed.docs` by both `serve` and `datamk context`.
    /// Defaults keep pre-ADR-0013 manifests parsing.
    #[serde(default)]
    pub docs: BTreeMap<String, crate::context::DocsFingerprint>,
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

/// The live-verify source-check record (issue #6, live-verify core):
/// `datamk verify` writes this (`.cell/source_check.json`, sibling of
/// `published.json`) after successfully checking every `materialize: never`
/// export against the live warehouse; `datamk context` reads it back to
/// populate `observed.source_check` and derive the `verified_at_source`
/// status.
///
/// `verify` and `context` run as separate processes, often in different CI
/// steps — this file is the only thing that carries a passed live check
/// from one to the other. `cell_yaml_digest` is the staleness key: `context`
/// embeds this record only when it matches the current `cell.yaml`'s own
/// digest (the same sha256 `context` stamps as `cell_yaml_digest` on every
/// emitted document) — a config edit between the verify step and the
/// context step must silently invalidate the record, never let a check of
/// the *previous* contract ride along as if it covered the current one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceCheckRecord {
    /// `"passed"` by construction: `verify` bails on any check failure
    /// before ever reaching the write, same discipline as
    /// `RunSummary.verify_outcome`.
    pub outcome: String,
    /// When the live check ran (RFC 3339, UTC).
    pub checked_at: String,
    /// When the checked data was last known-true, if a connector can supply
    /// that cheaply and truthfully. `None` in this slice — no connector
    /// currently threads one out of the bind path; never fabricated and
    /// never defaulted to `checked_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_as_of: Option<String>,
    pub datamk_version: String,
    pub cell_yaml_digest: String,
}

impl SourceCheckRecord {
    /// Read the record from a cell directory, if present and well-formed.
    /// Callers still must compare `cell_yaml_digest` against the current
    /// `cell.yaml` before trusting it — this only handles "the file exists
    /// and parses," not "the file is fresh."
    pub fn load(dir: &Path) -> Option<SourceCheckRecord> {
        let raw = std::fs::read_to_string(dir.join(".cell").join("source_check.json")).ok()?;
        serde_json::from_str(&raw).ok()
    }
}
