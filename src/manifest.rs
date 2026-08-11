use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
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
/// `published.json`) after successfully checking every bound export against
/// its declared source; `datamk context` reads it back to
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
/// `profile` is the second, independent gate: a record written by
/// `datamk verify -p local` must not silently validate `datamk context -p
/// prod` (or a hosted `prod` server) just because the digest still matches
/// — two profiles can read entirely different warehouses. Never on the
/// wire (`context::SourceCheck` doesn't map it) — used only to decide
/// whether this record applies to the profile currently reading it.
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
    /// The `--profile` the live check ran under. `#[serde(default)]` so a
    /// record written before this field existed still *parses* (as `""`,
    /// matching no real profile name) — it fails `fresh_for`'s profile
    /// check for every profile rather than failing to load at all, which is
    /// the fail-closed behavior we want, not a special case for it.
    #[serde(default)]
    pub profile: String,
}

impl SourceCheckRecord {
    /// Read the record from a cell directory, if present and well-formed.
    /// Callers still must compare `cell_yaml_digest`/`profile` against the
    /// current ones before trusting it — this only handles "the file exists
    /// and parses," not "the file is fresh." Prefer `fresh_for`.
    pub fn load(dir: &Path) -> Option<SourceCheckRecord> {
        let raw = std::fs::read_to_string(dir.join(".cell").join("source_check.json")).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// `load`, gated on staleness: `None` unless a record exists AND its
    /// `cell_yaml_digest` matches the caller's current one AND it was
    /// written under the same `profile`. The one place this check happens —
    /// `context::build_document` and `serve`'s startup load (issue #16)
    /// both call this rather than each re-implementing the match, so the
    /// two doors can never apply a different freshness rule to the same
    /// file.
    pub fn fresh_for(
        dir: &Path,
        cell_yaml_digest: &str,
        profile: &str,
    ) -> Option<SourceCheckRecord> {
        let r = Self::load(dir)?;
        if r.cell_yaml_digest != cell_yaml_digest {
            tracing::info!(
                "found .cell/source_check.json but its digest no longer matches cell.yaml — \
                 the config changed since the last `datamk verify`; omitting observed.source_check \
                 (re-run `datamk verify` for a current one)"
            );
            return None;
        }
        if r.profile != profile {
            tracing::info!(
                profile = %profile,
                record_profile = %r.profile,
                "found .cell/source_check.json but it was written under a different profile — \
                 a live check under one profile does not attest another; omitting \
                 observed.source_check (re-run `datamk verify -p {profile}` for a current one)"
            );
            return None;
        }
        Some(r)
    }
}

/// The observed upstream-column-description record (issue #6/#10):
/// `datamk verify`'s live-verify bind pass already reads `engine::
/// bind_sources`' third return value (`SourceWarehouseColumns`, added for
/// #9's type authority) — this is the same fact, persisted so `datamk
/// context`/hosted `/context` can surface it without their own warehouse
/// round trip. Written to `.cell/source_descriptions.json`, sibling of
/// `source_check.json`, with the identical fail-closed digest+profile
/// discipline (`fresh_for`) and the identical reason: `verify` and
/// `context`/`serve` are separate processes, so a live fact from one must
/// carry to the others through a file, gated on still applying to the
/// `cell.yaml`/profile currently being read.
///
/// Only BigQuery populates anything here today (ADR 0010: Postgres and
/// Snowflake run no metadata job, so `SourceWarehouseColumns.descriptions`
/// is always empty for them) — a source absent from `sources` means no
/// description was ever observed for it, not that its columns are
/// undocumented upstream; the two are indistinguishable from here, and this
/// record does not claim otherwise.
///
/// Upstream description text is untrusted (someone else's warehouse
/// comment) — carried exclusively through `serde`'s (de)serialization, never
/// through `format!`, so it can never be read as part of a message template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceDescriptionsRecord {
    pub written_at: String,
    pub datamk_version: String,
    pub cell_yaml_digest: String,
    /// Same second, independent gate as `SourceCheckRecord::profile` — see
    /// its doc comment. `#[serde(default)]` for the same forward-parse
    /// reason.
    #[serde(default)]
    pub profile: String,
    /// source name (as declared under `sources:`) -> column name -> upstream
    /// description. Only sources with at least one non-empty description
    /// are present — an empty inner map never appears (the write path below
    /// filters it), so presence itself is meaningful.
    pub sources: BTreeMap<String, IndexMap<String, String>>,
}

impl SourceDescriptionsRecord {
    /// Read the record from a cell directory, if present and well-formed —
    /// same "parses, not necessarily fresh" contract as
    /// `SourceCheckRecord::load`. Prefer `fresh_for`.
    pub fn load(dir: &Path) -> Option<SourceDescriptionsRecord> {
        let raw =
            std::fs::read_to_string(dir.join(".cell").join("source_descriptions.json")).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// `load`, gated on staleness — identical rule to
    /// `SourceCheckRecord::fresh_for` (digest match AND profile match), the
    /// one place this file's freshness gets decided so `context::
    /// build_document` and `serve`'s startup load can never apply a
    /// different rule to the same file.
    pub fn fresh_for(
        dir: &Path,
        cell_yaml_digest: &str,
        profile: &str,
    ) -> Option<SourceDescriptionsRecord> {
        let r = Self::load(dir)?;
        if r.cell_yaml_digest != cell_yaml_digest {
            tracing::info!(
                "found .cell/source_descriptions.json but its digest no longer matches \
                 cell.yaml — the config changed since the last `datamk verify`; omitting \
                 observed.source_descriptions (re-run `datamk verify` for a current one)"
            );
            return None;
        }
        if r.profile != profile {
            tracing::info!(
                profile = %profile,
                record_profile = %r.profile,
                "found .cell/source_descriptions.json but it was written under a different \
                 profile — a live check under one profile does not attest another; omitting \
                 observed.source_descriptions (re-run `datamk verify -p {profile}` for a \
                 current one)"
            );
            return None;
        }
        Some(r)
    }

    /// Write the record from `engine::bind_sources`' third return value,
    /// filtered to (a) sources named by some export's `bind:` — `def` is
    /// the one place that scoping lives, the same set `verify::check`'s
    /// `column_type_ok` already consults per export
    /// (`export.bind.as_ref().and_then(|bind| warehouse_columns.get(bind))`)
    /// — and (b) within those, sources that actually carry a description
    /// (empty inner maps and connectors with no metadata job contribute
    /// nothing — never an empty-map entry).
    ///
    /// `warehouse_columns` is populated for *every* connection source
    /// `bind_sources` classifies against a metadata job, materializing or
    /// bound — a private `crm` connection feeding only a materializing
    /// transform gets an entry exactly like a bound one does. Without the
    /// `def`-scoped filter, this record would publish that private source's
    /// full documented column set, including columns the interface never
    /// declares, from a source ADR 0012 §4 says appears nowhere on the
    /// wire — not even as a name. `observed.source_descriptions` exists to
    /// describe bound exports, nothing else.
    ///
    /// No-ops (writes nothing, returns `Ok`) when no bound source has any
    /// description, the same "leave nothing behind rather than a file with
    /// an empty `sources: {}`" discipline that keeps `fresh_for`'s absence
    /// check meaningful.
    pub fn write(
        dir: &Path,
        cell_yaml_digest: &str,
        profile: &str,
        def: &crate::config::CellDef,
        warehouse_columns: &HashMap<String, crate::engine::SourceWarehouseColumns>,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        use std::collections::HashSet;

        let bound_sources: HashSet<&str> = def
            .interface
            .iter()
            .filter_map(|e| e.bind.as_deref())
            .collect();

        let sources: BTreeMap<String, IndexMap<String, String>> = warehouse_columns
            .iter()
            .filter(|(name, wc)| {
                bound_sources.contains(name.as_str()) && !wc.descriptions.is_empty()
            })
            .map(|(name, wc)| (name.clone(), wc.descriptions.clone()))
            .collect();
        if sources.is_empty() {
            return Ok(());
        }

        let path = dir.join(".cell").join("source_descriptions.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let record = SourceDescriptionsRecord {
            written_at: crate::timeutil::rfc3339_utc(crate::timeutil::unix_now()),
            datamk_version: env!("CARGO_PKG_VERSION").to_string(),
            cell_yaml_digest: cell_yaml_digest.to_string(),
            profile: profile.to_string(),
            sources,
        };
        std::fs::write(&path, serde_json::to_string_pretty(&record)?)
            .with_context(|| format!("writing {}", path.display()))?;
        tracing::info!(path = %path.display(), "live-verify source descriptions recorded");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datamk-manifest-descriptions-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn wc(descriptions: &[(&str, &str)]) -> crate::engine::SourceWarehouseColumns {
        crate::engine::SourceWarehouseColumns {
            connector: "bigquery",
            columns: IndexMap::new(),
            descriptions: descriptions
                .iter()
                .map(|(c, d)| (c.to_string(), d.to_string()))
                .collect(),
        }
    }

    /// A minimal `CellDef` with one bound export per name in `bound` —
    /// enough for `write`'s `def.interface`-scoping filter, nothing else.
    fn bound_def(bound: &[&str]) -> crate::config::CellDef {
        let mut yaml = "cell: t\ninterface:\n".to_string();
        for (i, name) in bound.iter().enumerate() {
            yaml.push_str(&format!(
                "  - name: e{i}\n    version: 1.0.0\n    bind: {name}\n    schema: {{ id: bigint }}\n"
            ));
        }
        serde_yaml::from_str(&yaml).unwrap()
    }

    #[test]
    fn write_is_a_noop_when_no_source_has_any_description() {
        let dir = tempdir("noop");
        let mut warehouse = HashMap::new();
        // A source present in the map, but with no descriptions — the
        // connector ran a metadata job and found nothing worth carrying.
        warehouse.insert("raw".to_string(), wc(&[]));
        SourceDescriptionsRecord::write(&dir, "digest1", "local", &bound_def(&["raw"]), &warehouse)
            .unwrap();
        assert!(
            !dir.join(".cell/source_descriptions.json").exists(),
            "an all-empty warehouse map must leave no file behind, not one with sources: {{}}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_filters_out_sources_with_no_descriptions_but_keeps_those_with_some() {
        let dir = tempdir("filter");
        let mut warehouse = HashMap::new();
        warehouse.insert("documented".to_string(), wc(&[("col", "a sentence")]));
        warehouse.insert("undocumented".to_string(), wc(&[]));
        SourceDescriptionsRecord::write(
            &dir,
            "digest1",
            "local",
            &bound_def(&["documented", "undocumented"]),
            &warehouse,
        )
        .unwrap();
        let r = SourceDescriptionsRecord::load(&dir).expect("record written");
        assert_eq!(r.sources.len(), 1, "got: {:?}", r.sources);
        assert!(r.sources.contains_key("documented"));
        assert!(!r.sources.contains_key("undocumented"));
        assert_eq!(r.sources["documented"]["col"], "a sentence");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H2: `warehouse_columns` carries an entry for *every* connection
    /// source `bind_sources` classifies against a metadata job —
    /// materializing or bound. A source with real, non-empty descriptions
    /// that no export `bind:`s (a private materializing-only connection,
    /// e.g. `crm`) must never appear in the written record, even though
    /// its `SourceWarehouseColumns` entry looks identical to a bound one's.
    #[test]
    fn write_never_publishes_a_source_no_export_binds_even_with_real_descriptions() {
        let dir = tempdir("unbound-source-excluded");
        let mut warehouse = HashMap::new();
        warehouse.insert("pii".to_string(), wc(&[("email", "customer email")]));
        // `crm` has real descriptions too, but only feeds a materializing
        // transform — no export binds it.
        warehouse.insert(
            "crm".to_string(),
            wc(&[("ssn", "a column that must never reach the wire")]),
        );
        SourceDescriptionsRecord::write(&dir, "digest1", "local", &bound_def(&["pii"]), &warehouse)
            .unwrap();
        let r = SourceDescriptionsRecord::load(&dir).expect("record written");
        assert_eq!(r.sources.len(), 1, "got: {:?}", r.sources);
        assert!(r.sources.contains_key("pii"));
        assert!(
            !r.sources.contains_key("crm"),
            "an unbound source's descriptions must never be persisted, even when populated: \
             got {:?}",
            r.sources
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The full-noop counterpart to the test above: every source in the
    /// warehouse map has real descriptions, but none is named by any
    /// export's `bind:` — the record must not be written at all (same
    /// "leave nothing behind" discipline as the empty-descriptions case).
    #[test]
    fn write_is_a_noop_when_no_bound_source_has_any_description() {
        let dir = tempdir("noop-unbound");
        let mut warehouse = HashMap::new();
        warehouse.insert("crm".to_string(), wc(&[("ssn", "private, unbound")]));
        // No export binds "crm" — an empty interface, or one bound to
        // something else entirely.
        SourceDescriptionsRecord::write(
            &dir,
            "digest1",
            "local",
            &bound_def(&["some_other_source"]),
            &warehouse,
        )
        .unwrap();
        assert!(
            !dir.join(".cell/source_descriptions.json").exists(),
            "no bound source has a description — nothing should be written"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fresh_for_rejects_a_digest_mismatch() {
        let dir = tempdir("digest-mismatch");
        let mut warehouse = HashMap::new();
        warehouse.insert("raw".to_string(), wc(&[("col", "d")]));
        SourceDescriptionsRecord::write(&dir, "digest1", "local", &bound_def(&["raw"]), &warehouse)
            .unwrap();
        assert!(SourceDescriptionsRecord::fresh_for(&dir, "digest1", "local").is_some());
        assert!(
            SourceDescriptionsRecord::fresh_for(&dir, "digest2", "local").is_none(),
            "a cell.yaml edit since the write must silently invalidate the record"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fresh_for_rejects_a_profile_mismatch() {
        let dir = tempdir("profile-mismatch");
        let mut warehouse = HashMap::new();
        warehouse.insert("raw".to_string(), wc(&[("col", "d")]));
        SourceDescriptionsRecord::write(&dir, "digest1", "local", &bound_def(&["raw"]), &warehouse)
            .unwrap();
        assert!(
            SourceDescriptionsRecord::fresh_for(&dir, "digest1", "prod").is_none(),
            "a record written under `local` must not attest `prod`"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_returns_none_when_no_file_exists() {
        let dir = tempdir("missing");
        assert!(SourceDescriptionsRecord::load(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
