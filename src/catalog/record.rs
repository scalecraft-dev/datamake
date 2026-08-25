//! The sidecar `datamk sync` writes and every credential-light consumer
//! reads (ADR 0016 §5): `.cell/deployed_catalog.json`, on the
//! `SourceDescriptionsRecord` pattern — written by a build-side process
//! with state and warehouse credentials, read by `context`, `serve`,
//! `verify`, `release` with none, shipped inside the deploy artifact.
//!
//! Freshness has one more gate than its siblings: `fresh_for` keys on
//! `(cell_yaml_digest, profile)` like they do, but a discovered cell's
//! `cell.yaml` never changes when the SQLMesh project does, so the record
//! also carries `max_age_secs` and is stale past it — staleness must be
//! bounded and visible, not merely possible.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::ir::DeployedCatalog;

/// The default `discover.max_age` (profile) — two days: long enough to
/// survive a weekend without a scheduled sync, short enough that a record
/// nobody refreshes stops being served.
pub const DEFAULT_MAX_AGE_SECS: u64 = 48 * 3600;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployedCatalogRecord {
    pub written_at: String,
    pub datamk_version: String,
    pub cell_yaml_digest: String,
    pub profile: String,
    pub max_age_secs: u64,
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
    Expired { age_secs: u64, max_age_secs: u64 },
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
            Staleness::Expired {
                age_secs,
                max_age_secs,
            } => write!(
                f,
                ".cell/deployed_catalog.json is {} old, past the profile's max_age of {} — \
                 re-run `datamk sync`",
                human_duration(*age_secs),
                human_duration(*max_age_secs)
            ),
        }
    }
}

pub fn human_duration(secs: u64) -> String {
    if secs.is_multiple_of(86_400) && secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else if secs.is_multiple_of(3600) && secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs.is_multiple_of(60) && secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// `48h`, `7d`, `30m`, `90s`, or a bare number of seconds.
pub fn parse_duration(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = num
        .parse()
        .with_context(|| format!("invalid duration '{s}': expected e.g. 48h, 7d, 30m"))?;
    let mult = match unit.trim() {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        u => bail!("invalid duration '{s}': unknown unit '{u}' (use s, m, h, or d)"),
    };
    Ok(n * mult)
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

    /// `load`, gated: digest match, profile match, and age ≤ `max_age_secs`
    /// as of `now` (unix seconds). The one place freshness is decided, so
    /// `context`, `serve` and `sync` can never apply different rules to the
    /// same file.
    pub fn fresh_for(
        dir: &Path,
        cell_yaml_digest: &str,
        profile: &str,
        now: i64,
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
        let written = crate::timeutil::parse_rfc3339_utc(&r.written_at)
            .ok_or_else(|| Staleness::Unreadable(format!("bad written_at '{}'", r.written_at)))?;
        let age_secs = now.saturating_sub(written).max(0) as u64;
        if age_secs > r.max_age_secs {
            return Err(Staleness::Expired {
                age_secs,
                max_age_secs: r.max_age_secs,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_and_render() {
        assert_eq!(parse_duration("48h").unwrap(), 48 * 3600);
        assert_eq!(parse_duration("7d").unwrap(), 7 * 86_400);
        assert_eq!(parse_duration("30m").unwrap(), 1800);
        assert_eq!(parse_duration("90").unwrap(), 90);
        assert!(parse_duration("2w").is_err());
        assert!(parse_duration("h").is_err());
        assert_eq!(human_duration(48 * 3600), "2d");
        assert_eq!(human_duration(5400), "90m");
    }
}
