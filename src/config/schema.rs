use anyhow::{Context, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A cell definition — the public contract a user authors in `cell.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellDef {
    pub cell: String,
    /// External inputs, bound as session-local TEMP VIEWs before transforms run.
    /// A source is either a raw path/URI or a reference to another cell's table.
    #[serde(default)]
    pub sources: IndexMap<String, Source>,
    /// Private transform SQL files, executed in listed order.
    #[serde(default)]
    pub transforms: Vec<String>,
    /// The declared public surface — the export list.
    #[serde(default)]
    pub interface: Vec<Export>,
    /// Authorization policy for the serving plane (default-deny).
    #[serde(default)]
    pub access: Access,
}

/// Cell-level authorization. The serving plane exposes data only when `shareable`
/// is true; if `roles` is non-empty, callers must present a bearer token mapped to
/// one of those roles. Empty `roles` = open (but still gated by `shareable`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Access {
    #[serde(default)]
    pub shareable: bool,
    #[serde(default)]
    pub roles: Vec<String>,
}

/// One exported object: a versioned, governable view onto a lake table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Export {
    pub name: String,
    /// Semantic version. The route keys on MAJOR (e.g. `name@2`).
    pub version: String,
    /// Physical object in the lake (defaults to `name`). The seam between
    /// private internals and the public name.
    #[serde(default)]
    pub source: Option<String>,
    /// Grain columns: exposed as equality filters and uniqueness-checked by `verify`.
    #[serde(default)]
    pub grain: Vec<String>,
    /// Declared column -> type. Order is preserved (IndexMap).
    #[serde(default)]
    pub schema: IndexMap<String, String>,
    #[serde(default)]
    pub freshness: Option<String>,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default)]
    pub contract: Contract,
}

impl Export {
    pub fn source_object(&self) -> &str {
        self.source.as_deref().unwrap_or(&self.name)
    }

    pub fn major(&self) -> Result<u64> {
        let v = semver::Version::parse(&self.version).with_context(|| {
            format!(
                "invalid semver '{}' for export '{}'",
                self.version, self.name
            )
        })?;
        Ok(v.major)
    }

    /// Route key, e.g. `orders_daily@2`.
    pub fn route(&self) -> Result<String> {
        Ok(format!("{}@{}", self.name, self.major()?))
    }
}

/// Whether an export appears in the discoverable catalog. Decoupled from `Contract`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Private,
    #[default]
    Discoverable,
}

/// The one deliberate human promotion. `Supported` endpoints serve a pinned snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Contract {
    #[default]
    Experimental,
    Supported,
}

/// An external input. A raw file path/URI (`s3://…`, local) read directly,
/// another cell's managed DuckLake table read by name (versioned, governed), or
/// a warehouse table read through a named connection (ADR 0003).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Source {
    /// Raw path/URI; DuckDB reads it directly (Parquet/CSV/JSON, globs ok).
    Raw(String),
    /// A dependency on another cell. The reference name + table + version are
    /// contract (here); the upstream's location is supplied by the profile.
    Cell {
        /// Reference name; resolved to a location via the profile's `cells` map.
        cell: String,
        table: String,
        /// Optional snapshot to pin (omitted = latest).
        #[serde(default)]
        version: Option<u64>,
    },
    /// A warehouse table. Which table is contract (here); which project/account
    /// and credentials is environment, resolved via the profile's `connections`
    /// map. The table path's shape is validated per connector (BigQuery:
    /// `dataset.table`).
    Connection { connection: String, table: String },
}

/// The location of an upstream cell, supplied per environment by a profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellLocation {
    pub catalog: String,
    pub storage: String,
}

/// A binding profile: the environment-specific config for one target (local, prod,
/// …). Loaded from `profiles/<name>.yaml`, never from `cell.yaml` — the same cell
/// runs everywhere; only the profile differs. Values may use `${VAR}` for secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bindings {
    pub catalog: String,
    pub storage: String,
    /// Optional S3 connection. Required only when `storage` or a source is `s3://`
    /// and the AWS default credential chain is not sufficient.
    #[serde(default)]
    pub s3: Option<S3Binding>,
    /// Path to a JSON file mapping bearer token -> roles. Injected, never baked.
    /// Required only when `access.roles` is set.
    #[serde(default)]
    pub principals: Option<String>,
    /// Locations of upstream cell dependencies (referenced by name from `sources`).
    #[serde(default)]
    pub cells: IndexMap<String, CellLocation>,
    /// Named warehouse connections (referenced by name from `connection` sources).
    /// Environment config: the same cell reads a sandbox project in dev and the
    /// real one in prod.
    #[serde(default)]
    pub connections: IndexMap<String, Connection>,
}

/// One named warehouse connection, tagged by `type`. A closed enum: an unknown
/// type is a parse error naming the valid types. Every field is env-expandable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Connection {
    Bigquery(BigQueryConnection),
}

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
}

/// S3 connection settings. Each field is env-expandable. With no key/secret,
/// DuckDB's `credential_chain` provider is used (env vars, profiles, IAM roles).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Binding {
    #[serde(default)]
    pub region: Option<String>,
    /// Custom endpoint host for S3-compatible stores (MinIO, R2). Empty = AWS.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// `vhost` (default) or `path` (required by most S3-compatible stores).
    #[serde(default)]
    pub url_style: Option<String>,
    #[serde(default)]
    pub key_id: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub use_ssl: Option<bool>,
}

impl CellDef {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading cell definition {}", path.display()))?;
        serde_yaml::from_str(&raw)
            .with_context(|| format!("parsing cell definition {}", path.display()))
    }
}

impl Bindings {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading binding profile {} (create it, or pass --profile)",
                path.display()
            )
        })?;
        serde_yaml::from_str(&raw)
            .with_context(|| format!("parsing binding profile {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn export(name: &str, version: &str, source: Option<&str>) -> Export {
        Export {
            name: name.to_string(),
            version: version.to_string(),
            source: source.map(str::to_string),
            grain: vec![],
            schema: IndexMap::new(),
            freshness: None,
            visibility: Visibility::default(),
            contract: Contract::default(),
        }
    }

    #[test]
    fn source_object_defaults_to_name() {
        assert_eq!(export("orders", "1.0.0", None).source_object(), "orders");
        assert_eq!(
            export("orders", "1.0.0", Some("orders_daily")).source_object(),
            "orders_daily"
        );
    }

    #[test]
    fn major_extracts_the_major_version() {
        assert_eq!(export("o", "2.1.0", None).major().unwrap(), 2);
        assert_eq!(export("o", "0.9.3", None).major().unwrap(), 0);
    }

    #[test]
    fn major_rejects_non_semver() {
        let err = export("o", "v2", None).major().unwrap_err().to_string();
        assert!(err.contains("invalid semver"), "unexpected error: {err}");
    }

    #[test]
    fn route_keys_on_major() {
        assert_eq!(
            export("orders_daily", "2.1.0", None).route().unwrap(),
            "orders_daily@2"
        );
    }

    #[test]
    fn defaults_are_experimental_discoverable_and_deny() {
        assert_eq!(Visibility::default(), Visibility::Discoverable);
        assert_eq!(Contract::default(), Contract::Experimental);
        let access = Access::default();
        assert!(!access.shareable);
        assert!(access.roles.is_empty());
    }

    #[test]
    fn celldef_parses_full_yaml_with_both_source_kinds() {
        let yaml = r#"
cell: orders
sources:
  raw_orders: s3://acme/orders/*.parquet
  upstream:
    cell: other
    table: customers
    version: 3
transforms:
  - sql/stg.sql
  - sql/final.sql
interface:
  - name: orders_daily
    version: 2.1.0
    grain: [order_date, region]
    schema:
      order_date: date
      region: string
      revenue: decimal
access:
  shareable: true
  roles: [analyst]
"#;
        let def: CellDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(def.cell, "orders");
        assert_eq!(def.transforms, vec!["sql/stg.sql", "sql/final.sql"]);

        match def.sources.get("raw_orders").unwrap() {
            Source::Raw(uri) => assert_eq!(uri, "s3://acme/orders/*.parquet"),
            other => panic!("expected raw source, got {other:?}"),
        }
        match def.sources.get("upstream").unwrap() {
            Source::Cell {
                cell,
                table,
                version,
            } => {
                assert_eq!(cell, "other");
                assert_eq!(table, "customers");
                assert_eq!(*version, Some(3));
            }
            other => panic!("expected cell source, got {other:?}"),
        }

        let exp = &def.interface[0];
        assert_eq!(exp.route().unwrap(), "orders_daily@2");
        assert_eq!(exp.grain, vec!["order_date", "region"]);
        // IndexMap preserves declared column order.
        let cols: Vec<_> = exp.schema.keys().cloned().collect();
        assert_eq!(cols, vec!["order_date", "region", "revenue"]);
        assert!(def.access.shareable);
        assert_eq!(def.access.roles, vec!["analyst"]);
        // Unspecified fields fall back to defaults.
        assert_eq!(exp.visibility, Visibility::Discoverable);
        assert_eq!(exp.contract, Contract::Experimental);
    }

    #[test]
    fn celldef_parses_a_connection_source() {
        let yaml = r#"
cell: orders
sources:
  crm_accounts:
    connection: crm
    table: sales.accounts
"#;
        let def: CellDef = serde_yaml::from_str(yaml).unwrap();
        match def.sources.get("crm_accounts").unwrap() {
            Source::Connection { connection, table } => {
                assert_eq!(connection, "crm");
                assert_eq!(table, "sales.accounts");
            }
            other => panic!("expected connection source, got {other:?}"),
        }
    }

    #[test]
    fn bindings_parse_a_bigquery_connection() {
        let yaml = r#"
catalog: ./.cell/catalog.ducklake
storage: ./.cell/data
connections:
  crm:
    type: bigquery
    project: acme-prod-crm
    billing_project: acme-billing
    credentials: /etc/datamk/bq-key.json
"#;
        let b: Bindings = serde_yaml::from_str(yaml).unwrap();
        let Connection::Bigquery(bq) = b.connections.get("crm").unwrap();
        assert_eq!(bq.project, "acme-prod-crm");
        assert_eq!(bq.billing_project.as_deref(), Some("acme-billing"));
        assert_eq!(bq.credentials.as_deref(), Some("/etc/datamk/bq-key.json"));
    }

    #[test]
    fn bindings_reject_an_unknown_connection_type() {
        let yaml = r#"
catalog: c
storage: s
connections:
  crm:
    type: snowflake
    project: p
"#;
        let err = serde_yaml::from_str::<Bindings>(yaml)
            .unwrap_err()
            .to_string();
        assert!(err.contains("snowflake"), "unexpected error: {err}");
    }

    #[test]
    fn celldef_parses_minimal_yaml_with_defaults() {
        let def: CellDef = serde_yaml::from_str("cell: bare").unwrap();
        assert_eq!(def.cell, "bare");
        assert!(def.sources.is_empty());
        assert!(def.transforms.is_empty());
        assert!(def.interface.is_empty());
        assert!(!def.access.shareable);
    }

    #[test]
    fn bindings_parse_from_yaml() {
        let yaml = r#"
catalog: ./.cell/catalog.ducklake
storage: ./.cell/data
cells:
  other:
    catalog: /lake/other.ducklake
    storage: /lake/other/data
"#;
        let b: Bindings = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(b.catalog, "./.cell/catalog.ducklake");
        assert_eq!(b.storage, "./.cell/data");
        assert!(b.s3.is_none());
        let loc = b.cells.get("other").unwrap();
        assert_eq!(loc.catalog, "/lake/other.ducklake");
    }
}
