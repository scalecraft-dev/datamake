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

/// Watermarked-read config for a `connection` source (ADR 0005). Valid only on
/// `Source::Connection` — the cost problem this feature addresses (full
/// warehouse re-scans) lives in warehouse reads, not raw files or cell-to-cell
/// composition. Deliberately deserialized to deny unknown fields: a typo'd key
/// here (`incremenetal:`) would otherwise silently parse as a plain connection
/// source, running full scans forever while the author believes the cell is
/// incremental.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Incremental {
    /// Monotonic column to track (e.g. `updated_at`, an autoincrement id, an
    /// ingestion timestamp). A property of the data, so contract, not
    /// environment. Existence/type/nullability are validated at bind time
    /// (offline `verify` cannot see the live warehouse column).
    pub cursor: String,
    /// Optional trailing window re-delivered every run to catch late-arriving
    /// rows (`30m`, `2h`, `1d`). Parsed via `parse_duration` at resolve time.
    /// Accepts any YAML scalar here (not just a quoted string) so an unquoted
    /// `lookback: 2` reaches `parse_duration` as `"2"` and fails with our
    /// no-unit error, never a raw serde type-mismatch error.
    #[serde(default, deserialize_with = "de_lookback")]
    pub lookback: Option<String>,
}

fn de_lookback<'de, D>(deserializer: D) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<serde_yaml::Value> = Option::deserialize(deserializer)?;
    match value {
        None | Some(serde_yaml::Value::Null) => Ok(None),
        Some(serde_yaml::Value::String(s)) => Ok(Some(s)),
        Some(serde_yaml::Value::Number(n)) => Ok(Some(n.to_string())),
        Some(serde_yaml::Value::Bool(b)) => Ok(Some(b.to_string())),
        Some(other) => Err(serde::de::Error::custom(format!(
            "`lookback` must be a duration like `2h`, got {}",
            yaml_kind(&other)
        ))),
    }
}

/// Parse a duration string of the form `<integer><unit>` where unit is one of
/// `s`/`m`/`h`/`d` (seconds/minutes/hours/days). This is the ADR 0005-ratified
/// convention for future duration-valued fields (existing fields use plain
/// unit-suffixed integers like `retention_days`; duration strings exist
/// because lookback windows genuinely span mixed units).
pub fn parse_duration(s: &str) -> Result<std::time::Duration> {
    let invalid = || {
        anyhow::anyhow!(
            "`lookback: \"{s}\"` is not a valid duration — use an integer with a unit suffix: \
             s, m, h, or d (e.g. `30m`, `2h`, `1d`)."
        )
    };

    if s.is_empty() {
        return Err(invalid());
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!(
            "`lookback: \"{s}\"` has no unit — durations need a suffix: s, m, h, or d \
             (e.g. `2h`)."
        );
    }
    let (digits, suffix) = s.split_at(s.len() - 1);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(invalid());
    }
    if !matches!(suffix, "s" | "m" | "h" | "d") {
        return Err(invalid());
    }
    let n: u64 = digits.parse().map_err(|_| invalid())?;
    if n == 0 {
        anyhow::bail!(
            "`lookback: \"{s}\"` is zero. Omit `lookback` to read only rows past the \
             watermark, or give a non-zero window (e.g. `2h`)."
        );
    }
    let secs = match suffix {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => unreachable!("suffix already validated"),
    };
    Ok(std::time::Duration::from_secs(secs))
}

fn yaml_kind(v: &serde_yaml::Value) -> &'static str {
    match v {
        serde_yaml::Value::Null => "null",
        serde_yaml::Value::Bool(_) => "a bool",
        serde_yaml::Value::Number(_) => "a number",
        serde_yaml::Value::String(_) => "a string",
        serde_yaml::Value::Sequence(_) => "a list",
        serde_yaml::Value::Mapping(_) => "a mapping",
        serde_yaml::Value::Tagged(_) => "a tagged value",
    }
}

/// An external input. A raw file path/URI (`s3://…`, local) read directly,
/// another cell's managed DuckLake table read by name (versioned, governed), or
/// a warehouse table read through a named connection (ADR 0003).
///
/// `Deserialize` is implemented by hand below rather than derived with
/// `#[serde(untagged)]`: an untagged enum swallows field-level serde errors
/// behind "data did not match any variant of untagged enum Source", which is
/// useless for a malformed `incremental:` block (ADR 0005 §1). Dispatch is on
/// YAML shape instead — string, or a mapping keyed by `cell` or `connection` —
/// and each mapping shape denies unknown fields, closing the same typo hazard
/// `Incremental` closes. `Serialize` keeps the plain derive; round-tripping is
/// unaffected.
#[derive(Debug, Clone, Serialize)]
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
    Connection {
        connection: String,
        table: String,
        /// Optional watermarked-read config (ADR 0005).
        #[serde(default)]
        incremental: Option<Incremental>,
    },
}

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let value = serde_yaml::Value::deserialize(deserializer)?;
        match value {
            serde_yaml::Value::String(s) => Ok(Source::Raw(s)),
            serde_yaml::Value::Mapping(map) => {
                let has_cell = map.contains_key("cell");
                let has_connection = map.contains_key("connection");
                match (has_cell, has_connection) {
                    (true, true) => Err(D::Error::custom(
                        "a source cannot have both `cell` and `connection` keys — it is either \
                         a `{ cell, table }` reference to another cell, or a \
                         `{ connection, table }` reference to a warehouse table, never both",
                    )),
                    (true, false) => {
                        #[derive(Deserialize)]
                        #[serde(deny_unknown_fields)]
                        struct CellHelper {
                            cell: String,
                            table: String,
                            #[serde(default)]
                            version: Option<u64>,
                        }
                        let h: CellHelper = serde_yaml::from_value(serde_yaml::Value::Mapping(map))
                            .map_err(D::Error::custom)?;
                        Ok(Source::Cell {
                            cell: h.cell,
                            table: h.table,
                            version: h.version,
                        })
                    }
                    (false, true) => deserialize_connection(map).map_err(D::Error::custom),
                    (false, false) => Err(D::Error::custom(
                        "source must be a path string, a `{ cell, table }` map, or a \
                         `{ connection, table }` map",
                    )),
                }
            }
            other => Err(D::Error::custom(format!(
                "source must be a path string, a `{{ cell, table }}` map, or a \
                 `{{ connection, table }}` map, got {}",
                yaml_kind(&other)
            ))),
        }
    }
}

/// Hand-rolled (not derived) so the `incremental:` field can wrap the nested
/// `Incremental` error into ADR 0005's exact user-visible text, and so the
/// connection helper's own unknown-field error names the three valid keys
/// (rather than serde's generic "expected one of ..." list).
fn deserialize_connection(map: serde_yaml::Mapping) -> std::result::Result<Source, String> {
    let mut connection: Option<String> = None;
    let mut table: Option<String> = None;
    let mut incremental: Option<Incremental> = None;

    for (k, v) in map {
        let key = k
            .as_str()
            .ok_or_else(|| "a connection source's keys must be strings".to_string())?
            .to_string();
        match key.as_str() {
            "connection" => connection = Some(as_yaml_string(v, "connection")?),
            "table" => table = Some(as_yaml_string(v, "table")?),
            "incremental" => {
                let inc: Incremental = serde_yaml::from_value(v)
                    .map_err(|e| rewrite_incremental_error(&e.to_string()))?;
                incremental = Some(inc);
            }
            other => {
                return Err(format!(
                    "unknown field `{other}` — a connection source has `connection`, \
                     `table`, and optional `incremental`."
                ))
            }
        }
    }

    let connection = connection
        .ok_or_else(|| "a connection source is missing required field `connection`".to_string())?;
    let table =
        table.ok_or_else(|| "a connection source is missing required field `table`".to_string())?;
    Ok(Source::Connection {
        connection,
        table,
        incremental,
    })
}

fn as_yaml_string(v: serde_yaml::Value, field: &str) -> std::result::Result<String, String> {
    match v {
        serde_yaml::Value::String(s) => Ok(s),
        other => Err(format!(
            "`{field}` must be a string, got {}",
            yaml_kind(&other)
        )),
    }
}

/// Rewrite serde's generic `deny_unknown_fields`/missing-field text for
/// `Incremental` into ADR 0005's exact wording, naming the block (`incremental:`)
/// and, for missing `cursor`, the fix.
fn rewrite_incremental_error(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("unknown field `") {
        if let Some(end) = rest.find('`') {
            let field = &rest[..end];
            return format!(
                "unknown field `{field}` in `incremental:` — expected `cursor` or `lookback`."
            );
        }
    }
    if raw.starts_with("missing field `cursor`") {
        return "`incremental:` is missing required field `cursor`. Name the monotonic \
                column to track, e.g. `cursor: updated_at`."
            .to_string();
    }
    format!("`incremental:` {raw}")
}

/// The location of an upstream cell, supplied per environment by a profile.
/// Mode by presence (ADR 0004 §11): `catalog` present ⇒ attach the upstream's
/// catalog directly (local dev, self-managed); absent ⇒ published mode — the
/// upstream's catalog artifacts live under `<storage>/catalog/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellLocation {
    #[serde(default)]
    pub catalog: Option<String>,
    pub storage: String,
}

/// A binding profile: the environment-specific config for one target (local, prod,
/// …). Loaded from `profiles/<name>.yaml`, never from `cell.yaml` — the same cell
/// runs everywhere; only the profile differs. Values may use `${VAR}` for secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bindings {
    /// Mode by presence (ADR 0004 §11). Present ⇒ direct attach: a local
    /// `.ducklake` file path or a self-managed `sqlite:`/`postgres:` DSN —
    /// today's behavior, kept for local dev. Absent ⇒ **published-artifact
    /// mode**: the catalog derives from `storage` (`<storage>/catalog/`),
    /// which must be an object store. Deployed profiles omit this field.
    #[serde(default)]
    pub catalog: Option<String>,
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
    /// For temporary STS credentials (SSO sessions, assumed roles): the third
    /// piece of the triple. Meaningful only alongside `key_id`/`secret`, and
    /// expires with them — suited to dev loops, not long-lived deployments.
    #[serde(default)]
    pub session_token: Option<String>,
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
            Source::Connection {
                connection,
                table,
                incremental,
            } => {
                assert_eq!(connection, "crm");
                assert_eq!(table, "sales.accounts");
                assert!(incremental.is_none());
            }
            other => panic!("expected connection source, got {other:?}"),
        }
    }

    #[test]
    fn celldef_parses_a_connection_source_with_incremental_cursor_only() {
        let yaml = r#"
cell: orders
sources:
  crm_accounts:
    connection: crm
    table: sales.accounts
    incremental:
      cursor: updated_at
"#;
        let def: CellDef = serde_yaml::from_str(yaml).unwrap();
        match def.sources.get("crm_accounts").unwrap() {
            Source::Connection { incremental, .. } => {
                let inc = incremental.as_ref().unwrap();
                assert_eq!(inc.cursor, "updated_at");
                assert!(inc.lookback.is_none());
            }
            other => panic!("expected connection source, got {other:?}"),
        }
    }

    #[test]
    fn celldef_parses_incremental_cursor_and_lookback() {
        let yaml = r#"
cell: orders
sources:
  crm_accounts:
    connection: crm
    table: sales.accounts
    incremental:
      cursor: updated_at
      lookback: 2h
"#;
        let def: CellDef = serde_yaml::from_str(yaml).unwrap();
        match def.sources.get("crm_accounts").unwrap() {
            Source::Connection { incremental, .. } => {
                let inc = incremental.as_ref().unwrap();
                assert_eq!(inc.cursor, "updated_at");
                assert_eq!(inc.lookback.as_deref(), Some("2h"));
            }
            other => panic!("expected connection source, got {other:?}"),
        }
    }

    #[test]
    fn celldef_unquoted_lookback_int_errors_with_no_unit_text_not_a_serde_type_error() {
        let yaml = r#"
cell: orders
sources:
  crm_accounts:
    connection: crm
    table: sales.accounts
    incremental:
      cursor: updated_at
      lookback: 2
"#;
        // `lookback: 2` deserializes cleanly here (as the string "2") — the
        // no-unit error only fires when `parse_duration` runs at resolve time.
        let def: CellDef = serde_yaml::from_str(yaml).unwrap();
        match def.sources.get("crm_accounts").unwrap() {
            Source::Connection { incremental, .. } => {
                assert_eq!(incremental.as_ref().unwrap().lookback.as_deref(), Some("2"));
            }
            other => panic!("expected connection source, got {other:?}"),
        }
        let err = parse_duration("2").unwrap_err().to_string();
        assert!(err.contains("has no unit"), "unexpected error: {err}");
        assert!(!err.contains("invalid type"), "leaked a serde error: {err}");
    }

    #[test]
    fn incremental_unknown_field_errors_with_user_visible_text() {
        let yaml = r#"
cell: orders
sources:
  crm_accounts:
    connection: crm
    table: sales.accounts
    incremental:
      cursor: updated_at
      windw: 2h
"#;
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "unknown field `windw` in `incremental:` — expected `cursor` or `lookback`."
            ),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn incremental_missing_cursor_errors_with_user_visible_text() {
        let yaml = r#"
cell: orders
sources:
  crm_accounts:
    connection: crm
    table: sales.accounts
    incremental:
      lookback: 2h
"#;
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "`incremental:` is missing required field `cursor`. Name the monotonic column \
                 to track, e.g. `cursor: updated_at`."
            ),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn connection_source_typo_top_level_key_errors_with_user_visible_text() {
        let yaml = r#"
cell: orders
sources:
  crm_accounts:
    connection: crm
    table: sales.accounts
    incremenetal:
      cursor: updated_at
"#;
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "unknown field `incremenetal` — a connection source has `connection`, \
                 `table`, and optional `incremental`."
            ),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn source_with_both_cell_and_connection_keys_errors() {
        let yaml = r#"
cell: orders
sources:
  bad:
    cell: other
    connection: crm
    table: t
"#;
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("cannot have both `cell` and `connection` keys"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn source_with_neither_cell_nor_connection_key_errors() {
        let yaml = r#"
cell: orders
sources:
  bad:
    table: t
"#;
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "source must be a path string, a `{ cell, table }` map, or a \
                          `{ connection, table }` map"
            ),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn duration_grammar_rejections() {
        for bad in ["2w", "2.5h", "-2h", "1h30m", "", " ", "h", "2 h"] {
            let err = parse_duration(bad).unwrap_err().to_string();
            assert!(
                err.contains("is not a valid duration") || err.contains("has no unit"),
                "for '{bad}': unexpected error: {err}"
            );
        }
    }

    #[test]
    fn duration_rejects_zero() {
        let err = parse_duration("0h").unwrap_err().to_string();
        assert!(err.contains("is zero"), "unexpected error: {err}");
    }

    #[test]
    fn duration_rejects_bare_number() {
        let err = parse_duration("2").unwrap_err().to_string();
        assert!(err.contains("has no unit"), "unexpected error: {err}");
    }

    #[test]
    fn duration_parses_valid_forms() {
        assert_eq!(parse_duration("30s").unwrap().as_secs(), 30);
        assert_eq!(parse_duration("2m").unwrap().as_secs(), 120);
        assert_eq!(parse_duration("2h").unwrap().as_secs(), 7200);
        assert_eq!(parse_duration("1d").unwrap().as_secs(), 86_400);
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
        assert_eq!(b.catalog.as_deref(), Some("./.cell/catalog.ducklake"));
        assert_eq!(b.storage, "./.cell/data");
        assert!(b.s3.is_none());
        let loc = b.cells.get("other").unwrap();
        assert_eq!(loc.catalog.as_deref(), Some("/lake/other.ducklake"));
    }
}
