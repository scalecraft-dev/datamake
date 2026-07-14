//! BigQuery connection config: the `type: bigquery` shape in a profile's
//! `connections:` map, and its resolve-time (`${VAR}` expansion) logic.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::bindings::{expand, expand_opt, ResolvedConnection};

/// BigQuery connection settings. Auth is Google Application Default Credentials;
/// `credentials` optionally points ADC at a service-account key file (a path,
/// like `principals` — never a literal token). One connection ≡ one project;
/// cross-project reads are a second connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BigQueryConnection {
    /// The GCP project whose datasets are read.
    pub project: String,
    /// Where query/read costs land. Defaults to `project`.
    #[serde(default)]
    pub billing_project: Option<String>,
    /// Path to a service-account key file. Omitted = the ambient ADC chain.
    #[serde(default)]
    pub credentials: Option<String>,
    /// Scratch object-store prefix (e.g. `gs://acme-bq-staging/datamk-scratch`)
    /// for the oversized-jobs-result escape hatch (ADR 0006 §3a): when a
    /// view's jobs-API read exceeds BigQuery's ~10GB anonymous-result
    /// ceiling, the engine escalates to `EXPORT DATA`, writing parquet here
    /// instead. The warehouse identity needs `storage.objects.create` on
    /// this prefix (in addition to `bigquery.jobs.create`); datamk's reader
    /// needs read, and cleanup needs `storage.objects.delete`. Omitted ⇒ an
    /// oversized result is a hard error naming this field.
    #[serde(default)]
    pub staging_uri: Option<String>,
}

/// Resolve-time `${VAR}` expansion for a `type: bigquery` connection block.
pub fn resolve_bigquery(bq: &BigQueryConnection) -> Result<ResolvedConnection> {
    Ok(ResolvedConnection::Bigquery {
        project: expand(&bq.project)?,
        billing_project: expand_opt(&bq.billing_project)?,
        credentials: expand_opt(&bq.credentials)?,
        staging_uri: expand_opt(&bq.staging_uri)?,
    })
}
