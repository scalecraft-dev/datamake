//! The published run summary (`catalog/executions/<N>.run.json`): a
//! denormalized narration of one `run`, written best-effort alongside the
//! catalog artifact it describes (`engine::run`'s publish branch). Never
//! truth — the catalog snapshot is; this is what `status` reads to show
//! "what just happened" without downloading and attaching the artifact
//! itself. Direct-attach (non-published) mode has no `catalog/` prefix to
//! write this under, so it's skipped there entirely — the local record in
//! that mode is the run log (see `logging`).

use serde::{Deserialize, Serialize};

/// One published execution's run summary. Field names are a stable
/// contract (read by `status`, and potentially scripted against directly)
/// — do not rename without a care.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub execution: u64,
    /// `None` when the snapshot id genuinely couldn't be read cheaply —
    /// never blocks writing the rest of the summary.
    pub snapshot_id: Option<i64>,
    pub started_at: String,
    pub finished_at: String,
    pub datamk_version: String,
    /// Always `"passed"` today — `run` never reaches the publish branch
    /// this summary is written from unless `verify::check` already
    /// succeeded. Kept as an explicit field (not implied by the summary's
    /// mere existence) for forward compatibility.
    pub verify_outcome: String,
    pub sources: Vec<SourceRunInfo>,
    pub transforms: Vec<TransformRunInfo>,
}

/// One source's contribution. The connection-source fields (`connection`,
/// `kind`, `staged_rows`, `bytes_scanned`) are `None` for raw/cell sources,
/// which have no warehouse read to narrate — never a fabricated zero.
/// `connection` is the profile's connection *name* only — never
/// credentials, never a resolved project. URIs (raw sources) are profile
/// config, not a secret, and aren't carried here at all today (only
/// warehouse connection sources are — see `kind`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRunInfo {
    pub name: String,
    pub connection: Option<String>,
    /// `"table"` (Storage Read API, ADR 0003/0006), `"view"` (jobs API,
    /// ADR 0006), or `"query"` (author SQL, ADR 0007).
    pub kind: Option<String>,
    pub staged_rows: Option<u64>,
    /// From the ADR 0007 §4 dry-run preflight where one ran (`query:`
    /// sources only, today) — `None` for everything else, not zero.
    pub bytes_scanned: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformRunInfo {
    pub file: String,
    pub duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RunSummary {
        RunSummary {
            execution: 47,
            snapshot_id: Some(12),
            started_at: "2026-07-13T10:00:00Z".to_string(),
            finished_at: "2026-07-13T10:00:05Z".to_string(),
            datamk_version: "0.0.7".to_string(),
            verify_outcome: "passed".to_string(),
            sources: vec![
                SourceRunInfo {
                    name: "raw_spend_hourly".to_string(),
                    connection: Some("dw_silver".to_string()),
                    kind: Some("query".to_string()),
                    staged_rows: Some(1234),
                    bytes_scanned: Some(987_654),
                },
                SourceRunInfo {
                    name: "raw_orders".to_string(),
                    connection: None,
                    kind: None,
                    staged_rows: None,
                    bytes_scanned: None,
                },
            ],
            transforms: vec![TransformRunInfo {
                file: "sql/stg_orders.sql".to_string(),
                duration_ms: 42,
            }],
        }
    }

    #[test]
    fn run_summary_serializes_to_the_documented_shape() {
        let json = serde_json::to_string_pretty(&sample()).unwrap();
        let expected = r#"{
  "execution": 47,
  "snapshot_id": 12,
  "started_at": "2026-07-13T10:00:00Z",
  "finished_at": "2026-07-13T10:00:05Z",
  "datamk_version": "0.0.7",
  "verify_outcome": "passed",
  "sources": [
    {
      "name": "raw_spend_hourly",
      "connection": "dw_silver",
      "kind": "query",
      "staged_rows": 1234,
      "bytes_scanned": 987654
    },
    {
      "name": "raw_orders",
      "connection": null,
      "kind": null,
      "staged_rows": null,
      "bytes_scanned": null
    }
  ],
  "transforms": [
    {
      "file": "sql/stg_orders.sql",
      "duration_ms": 42
    }
  ]
}"#;
        assert_eq!(json, expected);
    }

    #[test]
    fn run_summary_round_trips_through_json() {
        let summary = sample();
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: RunSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.execution, summary.execution);
        assert_eq!(parsed.sources.len(), summary.sources.len());
        assert_eq!(parsed.sources[0].bytes_scanned, Some(987_654));
        assert_eq!(parsed.sources[1].kind, None);
    }

    /// Defense in depth against a future field addition leaking
    /// environment: the serialized shape must never carry anything
    /// credentials- or connection-string-shaped.
    #[test]
    fn run_summary_never_carries_a_credentials_shaped_field_name() {
        let json = serde_json::to_string(&sample()).unwrap();
        for banned in [
            "credential",
            "secret",
            "key_id",
            "billing_project",
            "password",
        ] {
            assert!(
                !json.to_lowercase().contains(banned),
                "run summary shape leaked a `{banned}`-shaped field: {json}"
            );
        }
    }
}
