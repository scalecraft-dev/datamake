//! The tool-agnostic intermediate representation of a modeling tool's
//! **deployed** models (ADR 0016 §8): everything downstream of this file —
//! selection, materialization into exports, the context document — knows
//! nothing about SQLMesh or dbt. One adapter per tool produces it; the
//! sidecar record (`catalog::record`) persists it.
//!
//! Every field a backend genuinely lacks is `Option` (dbt has no interval
//! store and no environment row); nothing else is — a backend must supply
//! the rest or fail loudly, never fabricate (ADR 0012 §2).

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// What proves "deployed". SQLMesh has an environment row a plan promoted
/// (`_environments[prod]`, with `plan_id` and `finalized_ts`); dbt's
/// `manifest.json` is the output of whatever invocation produced it — a
/// laptop's dev artifacts are byte-shaped like prod's. Surfaced verbatim in
/// the document so the requirement "never undeployed local edits" is met
/// for SQLMesh by construction and honestly qualified for dbt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    EnvironmentRow,
    ArtifactOnly,
}

/// Where a model's column definitions came from — recorded per model so the
/// document's `from` can say so (ADR 0015 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnsSource {
    /// The model declared `columns` in its definition.
    Declared,
    /// Read from the warehouse's `INFORMATION_SCHEMA` (or equivalent).
    Warehouse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployedCatalog {
    /// `"sqlmesh"` today.
    pub tool: String,
    pub environment: String,
    /// SQLMesh: the promoted plan; dbt: `metadata.invocation_id`.
    pub plan_id: String,
    /// SQLMesh: `finalized_ts` (an unfinalized environment is refused, §3);
    /// dbt: the artifact's `generated_at`.
    pub finalized_at: String,
    pub synced_at: String,
    pub evidence: Evidence,
    /// The tool's own state/artifact schema version, as it reports it.
    pub schema_version: String,
    /// Every model the adapter *selected*, in the tool's own order.
    pub models: Vec<DeployedModel>,
    /// How many models the environment held before selection, and how many
    /// were left out by it — so a reader of the record can tell "this cell
    /// is 5 of 1969" from "this project has 5 models".
    pub total_models: usize,
    pub unselected_models: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployedModel {
    /// Unquoted `catalog.schema.table` (or `schema.table`), the tool's own
    /// model name.
    pub name: String,
    /// `schema.table` — the **virtual** object consumers query in this
    /// environment, never the version-suffixed physical table.
    pub object: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<String>,
    /// The exact deployed snapshot: SQLMesh's snapshot identifier; dbt's
    /// file checksum. Opaque — a change token, never parsed.
    pub fingerprint: String,
    /// SQLMesh's data version (the physical-table version). dbt: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// SQLMesh's `data_hash` — moves iff the model's *data-affecting*
    /// definition changed. What `sync` refuses to overwrite silently for a
    /// `supported` export (ADR 0016 §6). dbt: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_hash: Option<String>,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Column name -> definition, in the tool's/warehouse's declared order.
    pub columns: IndexMap<String, DeployedColumn>,
    pub columns_source: ColumnsSource,
    #[serde(default)]
    pub grain: Vec<String>,
    /// Unquoted model names of this model's parents (one hop).
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Loaded `[start, end)` as RFC 3339, from SQLMesh's `_intervals`.
    /// `None` for tools with no interval store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intervals: Option<Interval>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_restatement: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployedColumn {
    /// Warehouse-native type as the source reports it (`INT64`, `NUMERIC`).
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where `description` came from: the tool's declaration (`Declared`,
    /// including inline comments the tool itself treats as declarations) or
    /// the warehouse's registered comment. `None` when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_source: Option<ColumnsSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interval {
    pub start: String,
    pub end: String,
}
