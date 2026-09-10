//! The sidecar `datamk sync` writes (ADR 0018 §4): `.cell/semantic_model.json`,
//! on the `SourceCheckRecord`/`SourceDescriptionsRecord` pattern
//! (`manifest.rs`) — written by a process that read a directory or a git
//! remote, read by every later consumer (a future verify/context/serve
//! phase, and the deploy artifact) with neither. Freshness is
//! `cell_yaml_digest` only, no profile: ADR 0018 §4, "`sync` needs no
//! profile for the semantic part."

use anyhow::{Context as _, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::source::SemanticModelSource;
use super::verify::DatasetCheck;
use super::Document;

/// Where the source resolved to at sync time — a local directory (absolute
/// path) or a git commit. Serializes flat (`{"dir": …}` / `{"commit": …}`),
/// matching ADR 0018 §4's example; both variants are written and read only
/// by datamk itself (never hand-authored), so `#[serde(untagged)]`'s only
/// real cost — losing per-field error detail on a malformed hand-written
/// document — never applies here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Resolved {
    Dir { dir: String },
    Commit { commit: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticModelRecord {
    pub datamk_version: String,
    pub cell_yaml_digest: String,
    pub synced_at: String,
    pub source: SemanticModelSource,
    pub resolved: Resolved,
    /// sha256 over the sorted, merged file bytes (`source::merge`).
    pub content_sha256: String,
    pub files: Vec<String>,
    /// semantic model name -> the relative file it was defined in
    /// (`source::merge`'s `Merged::model_files`) — carried onto the record
    /// so a later phase (ADR 0018 §7's `semantic_matches[]`) can name which
    /// file a model/dataset/field came from without re-walking the source.
    /// `#[serde(default)]`: a record written before this field existed
    /// still parses, as empty.
    #[serde(default)]
    pub model_files: IndexMap<String, String>,
    pub document: Document,
}

impl SemanticModelRecord {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(".cell").join("semantic_model.json")
    }

    pub fn save(&self, dir: &Path) -> Result<PathBuf> {
        let path = Self::path(dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    pub fn load(dir: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(Self::path(dir)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// `load`, gated on `cell_yaml_digest` matching — the one place
    /// staleness is decided, so every consumer applies the same rule (the
    /// `SourceCheckRecord::fresh_for` pattern, `manifest.rs`).
    pub fn fresh_for(dir: &Path, cell_yaml_digest: &str) -> Option<Self> {
        let r = Self::load(dir)?;
        if r.cell_yaml_digest != cell_yaml_digest {
            tracing::info!(
                "found .cell/semantic_model.json but its digest no longer matches cell.yaml — \
                 the config changed since the last `datamk sync`; omitting the semantic model \
                 (re-run `datamk sync` for a current one)"
            );
            return None;
        }
        Some(r)
    }
}

/// The `datamk verify` semantic-check record (ADR 0018 §6):
/// `.cell/semantic_check.json`, sibling of `.cell/source_check.json`
/// (`manifest::SourceCheckRecord`) and written under the identical
/// discipline — `cell_yaml_digest` and `profile` are both freshness gates
/// (`fresh_for`), since `verify` and a later `context`/`serve` phase run as
/// separate processes. `content_sha256` is the semantic model record's own
/// (`SemanticIndex::content_sha256`), not a hash of this file — it lets a
/// consumer tell "the Ossie source changed since this was checked" without
/// re-reading `semantic_model.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticCheckRecord {
    pub datamk_version: String,
    pub cell_yaml_digest: String,
    pub profile: String,
    pub checked_at: String,
    pub content_sha256: String,
    /// "model/dataset@route" (one entry per bound route; "model/dataset"
    /// with no suffix when unbound) -> what verify measured, bound and
    /// unbound datasets alike (ADR 0018 §5: an unbound dataset is kept,
    /// never dropped; a dataset bound to two routes gets two entries).
    pub datasets: BTreeMap<String, DatasetCheck>,
    /// "model/relationship name" -> `"verified"` or `"unbound"`.
    pub relationships: BTreeMap<String, String>,
    /// "model/metric name" -> `"verified"`, `"unverified:dialect"`, or
    /// `"unverified:unbound"`.
    pub metrics: BTreeMap<String, String>,
}

impl SemanticCheckRecord {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(".cell").join("semantic_check.json")
    }

    pub fn save(&self, dir: &Path) -> Result<PathBuf> {
        let path = Self::path(dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    pub fn load(dir: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(Self::path(dir)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// `load`, gated on digest AND profile matching — the `SourceCheckRecord
    /// ::fresh_for` pattern (`manifest.rs`): a check under one profile must
    /// not attest another, and a `cell.yaml` edit since the last `datamk
    /// verify` must silently drop the record rather than let a stale check
    /// ride along as current.
    pub fn fresh_for(dir: &Path, cell_yaml_digest: &str, profile: &str) -> Option<Self> {
        let r = Self::load(dir)?;
        if r.cell_yaml_digest != cell_yaml_digest {
            tracing::info!(
                "found .cell/semantic_check.json but its digest no longer matches cell.yaml — \
                 the config changed since the last `datamk verify`; omitting semantic_check \
                 (re-run `datamk verify` for a current one)"
            );
            return None;
        }
        if r.profile != profile {
            tracing::info!(
                profile = %profile,
                record_profile = %r.profile,
                "found .cell/semantic_check.json but it was written under a different profile — \
                 omitting semantic_check (re-run `datamk verify -p {profile}` for a current one)"
            );
            return None;
        }
        Some(r)
    }
}

/// ADR 0018 §3: `datamk release` and `datamk deploy` refuse a
/// `semantic_model.git` source whose `ref:` is not a 40-hex commit sha
/// matching the record's resolved commit — a moving branch (or an unset
/// `ref:`, which floats HEAD) never ships. A no-op for `dir:` sources and
/// cells with no `semantic_model:` at all.
pub fn check_release_pinned(dir: &Path, file: &Path, def: &crate::config::CellDef) -> Result<()> {
    let Some(SemanticModelSource::Git { r#ref, .. }) = &def.semantic_model else {
        return Ok(());
    };
    let digest = crate::context::cell_yaml_digest_of(file)?;
    let Some(record) = SemanticModelRecord::fresh_for(dir, &digest) else {
        anyhow::bail!(
            "`datamk release` refuses `semantic_model.git` — .cell/semantic_model.json is \
             missing or stale; run `datamk sync` first so what ships was actually synced."
        );
    };
    let Resolved::Commit { commit } = &record.resolved else {
        anyhow::bail!(
            "`datamk release` refuses `semantic_model.git` — .cell/semantic_model.json was not \
             synced from a git source; run `datamk sync` again."
        );
    };
    let is_pinned_sha = |s: &str| s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit());
    let ref_display = r#ref.as_deref().unwrap_or("(unset, defaults to HEAD)");
    match r#ref.as_deref() {
        Some(r) if is_pinned_sha(r) && r == commit => Ok(()),
        Some(r) if is_pinned_sha(r) => anyhow::bail!(
            "`datamk release` refuses `semantic_model.ref: {r}` — .cell/semantic_model.json was \
             synced at a different commit ({commit}); re-run `datamk sync` after updating \
             `ref:`, or set `ref: {commit}` to match what was last synced."
        ),
        _ => anyhow::bail!(
            "`datamk release` refuses `semantic_model.ref: {ref_display}` — it resolves to a \
             moving branch. .cell/semantic_model.json records commit {commit}; pin `ref: \
             {commit}` in cell.yaml so what ships is what was verified."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn semantic_check_sample(digest: &str, profile: &str) -> SemanticCheckRecord {
        SemanticCheckRecord {
            datamk_version: "0.0.0".to_string(),
            cell_yaml_digest: digest.to_string(),
            profile: profile.to_string(),
            checked_at: "2026-09-10T00:00:00Z".to_string(),
            content_sha256: "abc".to_string(),
            datasets: BTreeMap::from([(
                "invoice/flight_spend@flight_spend@1".to_string(),
                DatasetCheck {
                    model: "invoice".to_string(),
                    dataset: "flight_spend".to_string(),
                    route: Some("flight_spend@1".to_string()),
                    primary_key: "matches".to_string(),
                    fields: BTreeMap::new(),
                },
            )]),
            relationships: BTreeMap::from([("invoice/r".to_string(), "verified".to_string())]),
            metrics: BTreeMap::from([("invoice/total".to_string(), "verified".to_string())]),
        }
    }

    #[test]
    fn semantic_check_record_round_trips_through_save_and_load() {
        let dir = tempdir("semantic-check-roundtrip");
        let rec = semantic_check_sample("d1", "local");
        rec.save(&dir).unwrap();
        let loaded = SemanticCheckRecord::load(&dir).unwrap();
        assert_eq!(loaded.cell_yaml_digest, "d1");
        assert_eq!(
            loaded.datasets["invoice/flight_spend@flight_spend@1"]
                .route
                .as_deref(),
            Some("flight_spend@1")
        );
        assert_eq!(loaded.relationships["invoice/r"], "verified");
        assert_eq!(loaded.metrics["invoice/total"], "verified");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn semantic_check_record_fresh_for_gates_on_digest_and_profile() {
        let dir = tempdir("semantic-check-fresh");
        semantic_check_sample("d1", "local").save(&dir).unwrap();
        assert!(SemanticCheckRecord::fresh_for(&dir, "d1", "local").is_some());
        assert!(SemanticCheckRecord::fresh_for(&dir, "d2", "local").is_none());
        assert!(SemanticCheckRecord::fresh_for(&dir, "d1", "prod").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datamk-ossie-record-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample(digest: &str) -> SemanticModelRecord {
        SemanticModelRecord {
            datamk_version: "0.0.0".to_string(),
            cell_yaml_digest: digest.to_string(),
            synced_at: "2026-09-10T00:00:00Z".to_string(),
            source: SemanticModelSource::Dir {
                dir: "../osi".to_string(),
            },
            resolved: Resolved::Dir {
                dir: "/abs/osi".to_string(),
            },
            content_sha256: "abc".to_string(),
            files: vec!["m.yaml".to_string()],
            model_files: IndexMap::new(),
            document: Document {
                version: "0.1.1".to_string(),
                dialects: vec![],
                vendors: vec![],
                semantic_model: vec![],
            },
        }
    }

    #[test]
    fn round_trips_through_save_and_load() {
        let dir = tempdir("roundtrip");
        let rec = sample("d1");
        rec.save(&dir).unwrap();
        let loaded = SemanticModelRecord::load(&dir).unwrap();
        assert_eq!(loaded.cell_yaml_digest, "d1");
        assert_eq!(loaded.files, vec!["m.yaml".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fresh_for_matches_digest_and_rejects_mismatch() {
        let dir = tempdir("fresh");
        sample("d1").save(&dir).unwrap();
        assert!(SemanticModelRecord::fresh_for(&dir, "d1").is_some());
        assert!(SemanticModelRecord::fresh_for(&dir, "d2").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn cell_def(yaml: &str) -> crate::config::CellDef {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn write_record(dir: &Path, digest: &str, resolved: Resolved) {
        SemanticModelRecord {
            datamk_version: "0.0.0".to_string(),
            cell_yaml_digest: digest.to_string(),
            synced_at: "2026-09-10T00:00:00Z".to_string(),
            source: SemanticModelSource::Git {
                git: "https://example.com/x.git".to_string(),
                path: None,
                r#ref: None,
            },
            resolved,
            content_sha256: "abc".to_string(),
            files: vec!["m.yaml".to_string()],
            model_files: IndexMap::new(),
            document: Document {
                version: "0.1.1".to_string(),
                dialects: vec![],
                vendors: vec![],
                semantic_model: vec![],
            },
        }
        .save(dir)
        .unwrap();
    }

    #[test]
    fn check_release_pinned_is_a_noop_without_semantic_model() {
        let dir = tempdir("pin-noop");
        let def = cell_def("cell: c\n");
        std::fs::write(dir.join("cell.yaml"), "cell: c\n").unwrap();
        check_release_pinned(&dir, &dir.join("cell.yaml"), &def).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_release_pinned_is_a_noop_for_a_dir_source() {
        let dir = tempdir("pin-dir");
        let yaml = "cell: c\nsemantic_model:\n  dir: osi\n";
        std::fs::write(dir.join("cell.yaml"), yaml).unwrap();
        let def = cell_def(yaml);
        check_release_pinned(&dir, &dir.join("cell.yaml"), &def).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_release_pinned_refuses_a_missing_record() {
        let dir = tempdir("pin-missing");
        let yaml = "cell: c\nsemantic_model:\n  git: https://example.com/x.git\n  ref: \
                    4f2a91caaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n";
        std::fs::write(dir.join("cell.yaml"), yaml).unwrap();
        let def = cell_def(yaml);
        let err = check_release_pinned(&dir, &dir.join("cell.yaml"), &def).unwrap_err();
        assert!(err.to_string().contains("missing or stale"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_release_pinned_refuses_a_moving_ref() {
        let dir = tempdir("pin-moving");
        let yaml = "cell: c\nsemantic_model:\n  git: https://example.com/x.git\n  ref: main\n";
        std::fs::write(dir.join("cell.yaml"), yaml).unwrap();
        let digest = crate::context::sha256_hex(yaml.as_bytes());
        write_record(
            &dir,
            &digest,
            Resolved::Commit {
                commit: "4f2a91caaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            },
        );
        let def = cell_def(yaml);
        let err = check_release_pinned(&dir, &dir.join("cell.yaml"), &def).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("moving branch"), "{msg}");
        assert!(
            msg.contains("4f2a91caaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "{msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_release_pinned_accepts_a_matching_pinned_sha() {
        let dir = tempdir("pin-ok");
        let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let yaml =
            format!("cell: c\nsemantic_model:\n  git: https://example.com/x.git\n  ref: {sha}\n");
        std::fs::write(dir.join("cell.yaml"), &yaml).unwrap();
        let digest = crate::context::sha256_hex(yaml.as_bytes());
        write_record(
            &dir,
            &digest,
            Resolved::Commit {
                commit: sha.to_string(),
            },
        );
        let def = cell_def(&yaml);
        check_release_pinned(&dir, &dir.join("cell.yaml"), &def).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_release_pinned_refuses_a_sha_that_does_not_match_the_synced_commit() {
        let dir = tempdir("pin-mismatch");
        let ref_sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let synced_sha = "cccccccccccccccccccccccccccccccccccccccc";
        let yaml = format!(
            "cell: c\nsemantic_model:\n  git: https://example.com/x.git\n  ref: {ref_sha}\n"
        );
        std::fs::write(dir.join("cell.yaml"), &yaml).unwrap();
        let digest = crate::context::sha256_hex(yaml.as_bytes());
        write_record(
            &dir,
            &digest,
            Resolved::Commit {
                commit: synced_sha.to_string(),
            },
        );
        let def = cell_def(&yaml);
        let err = check_release_pinned(&dir, &dir.join("cell.yaml"), &def).unwrap_err();
        assert!(err.to_string().contains("different commit"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolved_serializes_flat() {
        let commit = Resolved::Commit {
            commit: "abc123".to_string(),
        };
        let json = serde_json::to_string(&commit).unwrap();
        assert_eq!(json, r#"{"commit":"abc123"}"#);
    }
}
