use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A cell definition — the public contract a user authors in `cell.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellDef {
    pub cell: String,
    /// One line: what this data product is (ADR 0012 §3). Ships in the
    /// context document and as `/openapi.json`'s `info.description`.
    /// Length-capped at parse time (`validate_prose`).
    #[serde(default)]
    pub description: Option<String>,
    /// A relative path to one long-form prose page, additive to
    /// `description` — never a replacement (ADR 0013). Delivered inline in
    /// the context document only under `GET /context?include=docs`; there
    /// is no `/docs/:name` route. Resolved and cap-checked at load
    /// (`validate_docs`, next to `validate_prose`), same failure discipline
    /// as `principals:`.
    #[serde(default)]
    pub docs: Option<String>,
    /// External inputs, bound as session-local TEMP VIEWs before transforms run.
    /// A source is either a raw path/URI or a reference to another cell's table.
    #[serde(default)]
    pub sources: IndexMap<String, Source>,
    /// Private transforms, executed in listed order: every entry is a
    /// SELECT-only file plus a `materialize:` strategy (ADR 0008) — the
    /// bare-path shorthand implies `replace`. There is no raw-DML entry.
    #[serde(default)]
    pub transforms: Vec<TransformEntry>,
    /// The declared public surface — the export list.
    #[serde(default)]
    pub interface: Vec<Export>,
    /// Authorization policy for the serving plane (default-deny).
    #[serde(default)]
    pub access: Access,
    /// ADR 0016: discover the interface from a modeling tool's deployed
    /// state instead of authoring it. Mutually exclusive with `sources:`,
    /// `transforms:` and `interface:` (`validate_discover`) — a discovered
    /// cell computes nothing and authors no export list; `datamk sync`
    /// materializes `interface`/`sources` from the sidecar record at load.
    #[serde(default)]
    pub discover: Option<Discover>,
    /// Set by `config::load` on a discovered cell from the sidecar record —
    /// never authored, never serialized back into `cell.yaml`.
    #[serde(skip)]
    pub discovered_from: Option<DiscoveredFrom>,
}

/// Where a claim came from (ADR 0015 §2). A **closed set** — an origin an
/// agent can't recognise is a bug, so this is an enum, never free text.
/// Extended by exactly one token per modeling-tool adapter as its ADR lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    /// Authored in `cell.yaml`.
    #[serde(rename = "cell.yaml")]
    CellYaml,
    /// Read from the warehouse's own metadata (a column comment observed by
    /// `datamk verify`'s live bind pass, or by `datamk sync`).
    #[serde(rename = "warehouse")]
    Warehouse,
    /// Declared in a SQLMesh model definition (ADR 0016) — including the
    /// inline column comments SQLMesh itself treats as declarations.
    #[serde(rename = "sqlmesh")]
    Sqlmesh,
}

/// `{ <field>: <origin> }` on a record — names every field of that record
/// that can originate in more than one place.
pub type FromMap = IndexMap<String, Origin>;

/// The `discover:` block (ADR 0016 §1).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Discover {
    pub from: DiscoverFrom,
    /// The tool environment to read — the deployed one, never a dev env.
    #[serde(default = "default_environment")]
    pub environment: String,
    /// `profiles/<p>.yaml` `connections.<name>`: the tool's state store.
    pub state: String,
    /// The schema the state tables live in (SQLMesh's default: `sqlmesh`).
    #[serde(default = "default_state_schema")]
    pub state_schema: String,
    /// `profiles/<p>.yaml` `connections.<name>`: where the models' objects
    /// live — types and registered comments are read from here.
    pub warehouse: String,
    /// REQUIRED to name at least one of `tags`/`schemas`/`models`: there is
    /// no "everything" default (VISION.md's explicit export list).
    #[serde(default)]
    pub select: Select,
    #[serde(default)]
    pub exclude: Exclude,
    #[serde(default)]
    pub on_unresolvable: OnUnresolvable,
    /// What `sync` does with an override whose model is not among the
    /// selected models — gone from the environment (renamed, moved to
    /// another schema) or excluded by `select`. An override is an authored
    /// promise about a named model; it never disappears silently.
    #[serde(default)]
    pub on_missing_override: OnMissingOverride,
    /// Refine a discovered model; never invent one.
    #[serde(default)]
    pub overrides: Vec<Override>,
}

fn default_environment() -> String {
    "prod".to_string()
}

fn default_state_schema() -> String {
    "sqlmesh".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiscoverFrom {
    Sqlmesh,
}

/// OR within a key, AND across keys. `kinds` defaults to every kind except
/// `EXTERNAL` (an `external_models.yaml` entry is an upstream, not a
/// product) and `SEED`; both are selectable explicitly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Select {
    #[serde(default)]
    pub tags: Vec<String>,
    /// `schema` names (the middle part of `catalog.schema.table`).
    #[serde(default)]
    pub schemas: Vec<String>,
    /// `schema.table` names.
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub kinds: Vec<String>,
}

impl Select {
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty() && self.schemas.is_empty() && self.models.is_empty()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exclude {
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub schemas: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// What `sync` does with a selected model whose columns cannot be resolved
/// (undeclared, and no warehouse row).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnUnresolvable {
    #[default]
    Fail,
    /// Drop it, and name it in the document's `notes[]`.
    Exclude,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnMissingOverride {
    #[default]
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Override {
    /// `schema.table`.
    pub model: String,
    /// The export name; default is the mangled `<schema>_<table>`.
    #[serde(default, rename = "as")]
    pub as_name: Option<String>,
    /// Authored semver. Required to promote to `contract: supported`.
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub contract: Option<Contract>,
    #[serde(default)]
    pub grain: Option<Vec<String>>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
    /// Wins over the tool's description; the tool's text is dropped, not
    /// carried alongside (one home per fact, ADR 0015 §2).
    #[serde(default)]
    pub description: Option<String>,
    /// A long-form consumer page for this export (ADR 0013), the one
    /// authored thing per export that has no home upstream — served under
    /// the export's route key exactly as for a hand-authored export. Same
    /// path rules and caps.
    #[serde(default)]
    pub docs: Option<String>,
}

/// The tool-side facts of one discovered export (ADR 0016 §7): a measured
/// block in the document (`deployed`), never in the digest. Set by
/// `config::load` from the sidecar record; never authored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredExport {
    /// Unquoted model name in the tool.
    pub model: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intervals: Option<crate::catalog::ir::Interval>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_restatement: Option<bool>,
    /// Route keys of selected parents.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// How many parents selection left out — a count, never their names.
    pub depends_on_unselected: usize,
    /// `synced_at` — the measurement's timestamp.
    pub at: String,
}

/// The cell-level discovery stamp (ADR 0016 §7).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredFrom {
    pub tool: String,
    pub environment: String,
    pub plan_id: String,
    pub finalized_at: String,
    pub synced_at: String,
    pub evidence: crate::catalog::ir::Evidence,
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
///
/// Deserialized to deny unknown fields (same discipline as `Incremental`,
/// below): a typo'd key — `binds:` for `bind:` being the motivating one —
/// would otherwise silently parse as a plain materialized export (`bind`
/// stays `None`, `#[serde(default)]`), reading its transform table under
/// `name` instead of binding the intended source, with no error anywhere
/// near the typo.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Export {
    pub name: String,
    /// Semantic version. The route keys on MAJOR (e.g. `name@2`).
    pub version: String,
    /// The transform table this export reads (defaults to `name`). The seam
    /// between private internals and the public name — a lake object, always
    /// (a bound export, below, has no transform table and leaves this
    /// unset). Ignored when `bind` is set; resolve-time-rejected if both are
    /// (`verify::validate_bound_exports`).
    #[serde(default)]
    pub source: Option<String>,
    /// Bind this export directly to an existing object declared in
    /// `sources:`, instead of a private transform (issue #6, binding model —
    /// replaces the removed `materialize: never` strategy). No SQL datamk
    /// ever runs: the object already exists — a warehouse table/view via a
    /// `connection:` source, or a raw file/glob — and `verify` checks the
    /// declared contract against it live, every time. `serve` never routes
    /// it (`data.served_here` and the route's own 404 say so); `datamk
    /// context`/hosted `/context` still describe it in full. Resolve-time
    /// validated (`verify::validate_bound_exports`): the named source must
    /// exist, must not be a `query:`-shaped connection source (ad hoc SQL
    /// nobody runs is the exact failure this replaces, not an existing
    /// object to point at), and `source` must be unset.
    #[serde(default)]
    pub bind: Option<String>,
    /// One or two sentences: what one row means (ADR 0012 §3). Required
    /// (non-empty) once the export is `contract: supported` — the lint
    /// lands on the deliberate promotion gesture, enforced by `verify`.
    #[serde(default)]
    pub description: Option<String>,
    /// A relative path to one long-form prose page for this export,
    /// additive to `description` — never a replacement, and never a
    /// substitute for it: `docs:` does not satisfy the `contract: supported`
    /// description lint (`verify`, ADR 0013).
    #[serde(default)]
    pub docs: Option<String>,
    /// Grain columns: exposed as equality filters and uniqueness-checked by `verify`.
    #[serde(default)]
    pub grain: Vec<String>,
    /// Declared column -> spec. Order is preserved (IndexMap). The value is
    /// either a bare type string (`region: string` — every pre-ADR-0012 cell
    /// parses as-is) or a mapping `{type, unit, description}` (ADR 0012 §3).
    #[serde(default)]
    pub schema: IndexMap<String, ColumnSpec>,
    #[serde(default)]
    pub freshness: Option<String>,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default)]
    pub contract: Contract,
    /// Origin of `description`/`grain` when not `cell.yaml` (ADR 0015 §2).
    /// Empty ⇒ every present field is authored. Never read from or
    /// written to `cell.yaml`; set by discovery (`config::load`).
    #[serde(skip)]
    pub from: FromMap,
    /// ADR 0016: present iff this export was discovered from a tool.
    #[serde(skip)]
    pub discovered: Option<DiscoveredExport>,
}

impl Export {
    pub fn source_object(&self) -> &str {
        self.source.as_deref().unwrap_or(&self.name)
    }

    /// Whether this export is bound (issue #6, binding model) rather than
    /// backed by a private transform — the one place this distinction is
    /// made; every consumer (`context`, `serve`, `release`, `verify`) reads
    /// this instead of re-deriving it from a transform-table lookup.
    pub fn is_bound(&self) -> bool {
        self.bind.is_some()
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

/// One declared column: its type, and optionally its meaning (ADR 0012 §3).
/// Exactly two meaning fields, admitted under the rationing rule — not
/// machine-derivable AND wrongness produces a confidently wrong number:
/// `unit` (structured, e.g. `USD` — the #1 silent-wrong-number source) and
/// `description` (non-obvious columns only). Prose that lives beside the
/// schema it describes, versions with it, and ships in the same artifact has
/// no independent lifecycle to rot on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSpec {
    /// The declared type — the same strings the bare shape always accepted.
    pub ty: String,
    /// Structured unit token (`USD`, `ms`, `rows`) — never prose.
    pub unit: Option<String>,
    pub description: Option<String>,
    /// Origin of each present field when not `cell.yaml` (ADR 0015 §2).
    /// Empty ⇒ authored. Never part of the YAML shape; set by discovery.
    pub from: FromMap,
}

impl ColumnSpec {
    /// A bare type with no meaning fields — what every pre-ADR-0012 schema
    /// entry parses to.
    pub fn bare(ty: &str) -> Self {
        ColumnSpec {
            ty: ty.to_string(),
            unit: None,
            description: None,
            from: FromMap::new(),
        }
    }
}

/// Dispatch on YAML shape, per the `Source` precedent — not
/// `#[serde(untagged)]`, which swallows field-level errors behind "data did
/// not match any variant". A string is a bare type; a mapping is
/// `{type, unit, description}` with unknown fields denied (a typo'd
/// `descripton:` must fail loud, not silently drop the sentence).
impl<'de> Deserialize<'de> for ColumnSpec {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let value = serde_yaml::Value::deserialize(deserializer)?;
        match value {
            serde_yaml::Value::String(s) => Ok(ColumnSpec::bare(&s)),
            serde_yaml::Value::Mapping(map) => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Helper {
                    #[serde(rename = "type")]
                    ty: String,
                    #[serde(default)]
                    unit: Option<String>,
                    #[serde(default)]
                    description: Option<String>,
                }
                let h: Helper = serde_yaml::from_value(serde_yaml::Value::Mapping(map))
                    .map_err(|e| D::Error::custom(rewrite_column_error(&e.to_string())))?;
                Ok(ColumnSpec {
                    ty: h.ty,
                    unit: h.unit,
                    description: h.description,
                    from: FromMap::new(),
                })
            }
            other => Err(D::Error::custom(format!(
                "a schema column must be a type string (`region: string`) or a mapping \
                 `{{ type, unit, description }}`, got {}",
                yaml_kind(&other)
            ))),
        }
    }
}

/// Round-trip cleanly: a spec with no meaning fields serializes back to the
/// bare string it was authored as; only a spec carrying meaning becomes a
/// mapping.
impl Serialize for ColumnSpec {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap as _;
        if self.unit.is_none() && self.description.is_none() {
            return serializer.serialize_str(&self.ty);
        }
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("type", &self.ty)?;
        if let Some(u) = &self.unit {
            map.serialize_entry("unit", u)?;
        }
        if let Some(d) = &self.description {
            map.serialize_entry("description", d)?;
        }
        map.end()
    }
}

/// Name the valid keys instead of serde's generic "expected one of" list.
fn rewrite_column_error(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("unknown field `") {
        if let Some(end) = rest.find('`') {
            let field = &rest[..end];
            return format!(
                "unknown field `{field}` in a schema column — a column mapping has `type`, \
                 and optional `unit` and `description`."
            );
        }
    }
    if raw.starts_with("missing field `type`") {
        return "a schema column mapping is missing required field `type` — the declared \
                type, e.g. `type: decimal`."
            .to_string();
    }
    format!("schema column: {raw}")
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
    /// A warehouse object read through a named connection. Which
    /// project/account and credentials is environment, resolved via the
    /// profile's `connections` map. Exactly one of `table`/`query` (ADR
    /// 0007 §1), enforced in `deserialize_connection` below:
    /// - `table`: a table path, validated per connector (BigQuery:
    ///   `dataset.table`), routed by object-kind classification (ADR 0006).
    /// - `query`: author-owned, warehouse-dialect SQL executed server-side
    ///   (ADR 0007) — the same trust tier as a transform, never parsed or
    ///   rewritten by the engine, and jobs-routed by construction.
    Connection {
        connection: String,
        #[serde(default)]
        table: Option<String>,
        #[serde(default)]
        query: Option<String>,
        /// Optional watermarked-read config (ADR 0005). Refused on a
        /// `query:` source at resolve time (ADR 0007 §3).
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
/// `Incremental` error into ADR 0005's exact user-visible text, the
/// connection helper's own unknown-field error names the valid keys (rather
/// than serde's generic "expected one of ..." list), and `table`/`query`'s
/// exactly-one-of (ADR 0007 §1) gets an error naming both fields instead of
/// serde's generic "missing field" for whichever one a typo'd author omitted.
fn deserialize_connection(map: serde_yaml::Mapping) -> std::result::Result<Source, String> {
    let mut connection: Option<String> = None;
    let mut table: Option<String> = None;
    let mut query: Option<String> = None;
    let mut incremental: Option<Incremental> = None;

    for (k, v) in map {
        let key = k
            .as_str()
            .ok_or_else(|| "a connection source's keys must be strings".to_string())?
            .to_string();
        match key.as_str() {
            "connection" => connection = Some(as_yaml_string(v, "connection")?),
            "table" => table = Some(as_yaml_string(v, "table")?),
            "query" => query = Some(as_yaml_string(v, "query")?),
            "incremental" => {
                let inc: Incremental = serde_yaml::from_value(v)
                    .map_err(|e| rewrite_incremental_error(&e.to_string()))?;
                incremental = Some(inc);
            }
            other => {
                return Err(format!(
                    "unknown field `{other}` — a connection source has `connection`, one of \
                     `table`/`query`, and optional `incremental`."
                ))
            }
        }
    }

    let connection = connection
        .ok_or_else(|| "a connection source is missing required field `connection`".to_string())?;

    // ADR 0007 §1: `table:`/`query:` are exactly-one-of. Checked here, not
    // left to two independent "missing field" errors, so both the
    // both-present and neither-present mistakes get one message naming both
    // fields and the reason they're mutually exclusive.
    let (table, query) = match (table, query) {
        (Some(_), Some(_)) => {
            return Err(
                "a connection source cannot have both `table` and `query` — it reads either a \
                 warehouse table path (`table:`), routed by object-kind classification, or \
                 author-owned server-side SQL (`query:`), executed as-is, never both."
                    .to_string(),
            )
        }
        (None, None) => {
            return Err(
                "a connection source is missing `table` or `query` — name the warehouse table \
                 path to read (`table: dataset.table`), or provide server-side SQL to run \
                 (`query: SELECT ...`)."
                    .to_string(),
            )
        }
        (table, query) => (table, query),
    };

    Ok(Source::Connection {
        connection,
        table,
        query,
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

/// `[A-Za-z_][A-Za-z0-9_]*` — a bare identifier, no dots/quotes/spaces.
/// Shared by every place a `cell.yaml` field names a column or table that
/// later reaches SQL as a double-quoted identifier: an incremental cursor
/// (`bindings::resolve_incremental`), a `materialize:` `key:` column, and a
/// declarative transform's resolved table name. Resolve-time shape
/// validation is defense in depth here, not the primary control — the
/// double-quote at the SQL build site is (ADR 0005 §1, ADR 0008 §7).
pub fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The three closed declarative materialization strategies a transform can
/// use (ADR 0008 §3) — every value replay-safe by construction, never by
/// author discipline. `append`/`upsert` require `key:` and are replay-safe
/// *unconditionally* (reconciled against existing state, invariant under
/// whether the SELECT yields a delta or a complete relation). `replace`
/// forbids `key:` (nothing to reconcile against) and is replay-safe only
/// *structurally* — the engine admits it solely in cells with no incremental
/// source (`resolve_declarative_transforms`'s incremental-source gate,
/// `config::mod::load`), never by trusting the SELECT is a complete
/// relation.
///
/// `Never` is **not a fourth strategy** — it is a rejected legacy value,
/// kept representable here only so `materialize: never` still *parses* far
/// enough for `verify::check_no_materialize_never` (called from
/// `config::mod::load`, with the whole `CellDef` in view) to reject it with
/// a migration error naming the affected export(s) and both exits
/// (materialize, or bind — see `Export::bind`). No code past that point ever
/// sees a `ResolvedTransform` with this strategy: it is issue #6's original
/// design (bind the SELECT as a session-local `TEMP VIEW` and stop), founder
/// -reversed once every shipped `never` fixture turned out to be `SELECT *`
/// — every semantic a real transform could add (a rename, a derived column,
/// a `WHERE`) was a promise nothing ran, which on a PII surface is silent
/// over-disclosure. A virtual export now binds directly to an existing
/// object instead; there is no transform at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MaterializeStrategy {
    /// Insert delta rows whose key is not already present (anti-join).
    /// Existing rows are never touched.
    Append,
    /// A new delivery replaces the stored row for its key (`MERGE`,
    /// primitive per ADR 0008 §5).
    Upsert,
    /// Rebuild the table from scratch every run (`CREATE OR REPLACE TABLE`,
    /// one statement). No `key:` — there is no prior state to reconcile
    /// against. Legal only in a cell with no incremental source (ADR 0008
    /// §3): a `replace` whose SELECT read an incremental delta would
    /// replace accumulated history with the delta — this ADR's founding
    /// incident.
    Replace,
    /// Rejected legacy value — see the enum's doc comment. Kept parseable,
    /// never executable.
    Never,
}

impl std::fmt::Display for MaterializeStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MaterializeStrategy::Append => "append",
            MaterializeStrategy::Upsert => "upsert",
            MaterializeStrategy::Replace => "replace",
            MaterializeStrategy::Never => "never",
        })
    }
}

/// A `transforms:` entry (ADR 0008, "There is one language for transforms"):
/// a SELECT-only file, always — either the bare-path shorthand (§ decision
/// 3: implicitly `materialize: replace`, no key, the default strategy) or
/// the full `materialize:` mapping (any of the three strategies, `key:`
/// required for `upsert`/`append`). **There is no raw-DML entry** — a prior
/// design that kept hand-written DML files as a coequal "escape hatch" was
/// field-tested and killed: two file contracts in one list was itself the
/// dominant confusion source. Both shapes here resolve through the exact
/// same DML-composition path (`resolve_transforms` below); `Path` is a
/// syntax shorthand, never a semantically different entry.
///
/// `Deserialize` is hand-rolled (not `#[serde(untagged)]`) for the same
/// reason as `Source`: dispatch on YAML shape (string vs mapping), so a
/// malformed mapping (a typo'd `materalize:`) gets an error naming the valid
/// fields instead of serde's generic "data did not match any variant".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum TransformEntry {
    /// The bare-path shorthand — `- sql/spend_daily.sql` — for
    /// `{ sql: sql/spend_daily.sql, materialize: replace }`. Table = file
    /// stem, no key, rebuilt from scratch every run: the default strategy
    /// for the common case (derived tables, rollups, dims).
    Path(String),
    /// The explicit `materialize:` mapping — required to select `upsert`/
    /// `append` (both need `key:`), and legal (if redundant) for an explicit
    /// `materialize: replace` too.
    Materialize {
        /// Path to the SELECT-only file, resolved against the cell
        /// directory like the bare-path shorthand.
        sql: String,
        materialize: MaterializeStrategy,
        /// Non-empty list of column identifiers, shaped like `grain:`.
        /// Required for `upsert`/`append`; forbidden for `replace`.
        key: Vec<String>,
        // No `table:` override (ADR 0008, "table = file stem... no
        // override"): every transform's table is its file's stem, no
        // exceptions. Rename the file if you want a different table name.
    },
}

impl TransformEntry {
    /// The SQL file path, regardless of variant — lets callers that only
    /// need the file (artifact collection, the run loop's file read) skip
    /// matching the enum themselves.
    pub fn file_path(&self) -> &str {
        match self {
            TransformEntry::Path(p) => p,
            TransformEntry::Materialize { sql, .. } => sql,
        }
    }
}

impl<'de> Deserialize<'de> for TransformEntry {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let value = serde_yaml::Value::deserialize(deserializer)?;
        match value {
            serde_yaml::Value::String(s) => Ok(TransformEntry::Path(s)),
            serde_yaml::Value::Mapping(map) => {
                deserialize_materialize_entry(map).map_err(D::Error::custom)
            }
            other => Err(D::Error::custom(format!(
                "a transforms entry must be a path string (a SELECT-only file, `materialize: \
                 replace` implied) or a mapping with `sql:`, `materialize:`, and `key:`, got {}",
                yaml_kind(&other)
            ))),
        }
    }
}

/// Hand-rolled so an unknown field names the valid ones (rather than serde's
/// generic `deny_unknown_fields` list) and a bad `materialize:` value names
/// the closed set instead of a generic enum-variant error.
fn deserialize_materialize_entry(
    map: serde_yaml::Mapping,
) -> std::result::Result<TransformEntry, String> {
    let mut sql: Option<String> = None;
    let mut materialize: Option<String> = None;
    let mut key: Option<Vec<String>> = None;

    for (k, v) in map {
        let field = k
            .as_str()
            .ok_or_else(|| "a transforms entry's keys must be strings".to_string())?
            .to_string();
        match field.as_str() {
            "sql" => sql = Some(as_yaml_string(v, "sql")?),
            "materialize" => materialize = Some(as_yaml_string(v, "materialize")?),
            "key" => key = Some(as_yaml_string_list(v, "key")?),
            "table" => {
                return Err(
                    "unknown field `table` — a declarative transform entry has `sql`, \
                     `materialize`, `key`. There is no `table:` override (ADR 0008 §2): every \
                     transform's table is its file's stem, raw and declarative alike. Rename \
                     the file if you want a different table name."
                        .to_string(),
                )
            }
            other => {
                return Err(format!(
                    "unknown field `{other}` — a declarative transform entry has `sql`, \
                     `materialize`, `key`."
                ))
            }
        }
    }

    let sql = sql.ok_or_else(|| {
        "a declarative transform entry is missing required field `sql` — name the SELECT-only \
         file to materialize, e.g. `sql: sql/fct_flights.sql`."
            .to_string()
    })?;
    let materialize_raw = materialize.ok_or_else(|| {
        format!(
            "transform '{sql}': missing required field `materialize` — one of \
             `append`/`upsert`/`replace`/`never`."
        )
    })?;
    let materialize = match materialize_raw.as_str() {
        "append" => MaterializeStrategy::Append,
        "upsert" => MaterializeStrategy::Upsert,
        "replace" => MaterializeStrategy::Replace,
        "never" => MaterializeStrategy::Never,
        // The most likely typo once `never` ships (issue #6): `view` is the
        // issue's original proposal, rejected twice over — it's a warehouse
        // noun for "an object I create" (the inverse of what this strategy
        // does), and `SourceRunInfo.kind` already uses `"view"` to mean "read
        // via the BigQuery jobs API" (`engine::run_summary`). Named ahead of
        // the generic branch below so the author lands on `never` directly
        // instead of guessing from a closed-set list.
        "view" => {
            return Err(format!(
                "transform '{sql}': `materialize: view` is not a recognized strategy — datamk \
                 creates no warehouse objects. Use `materialize: never` to leave the rows where \
                 they are (the contract is still verified against them), or \
                 `append`/`upsert`/`replace` to hold them."
            ))
        }
        other => {
            return Err(format!(
                "transform '{sql}': `materialize: {other}` is not a recognized strategy — use \
                 `append`, `upsert`, `replace`, or `never`."
            ))
        }
    };

    // `key:` is required for append/upsert (they reconcile against prior
    // state by key) and forbidden for replace/never (ADR 0008 §3, issue #6:
    // neither has a prior stored row to reconcile a key against, so a
    // `key:` next to either is meaningless config, not harmless config).
    let key = match materialize {
        MaterializeStrategy::Replace => {
            if key.is_some() {
                return Err(format!(
                    "transform '{sql}': `key:` is not allowed with `materialize: replace` — \
                     replace rebuilds the table from scratch every run, so there is nothing to \
                     reconcile a key against. Remove `key:`, or use `materialize: \
                     upsert`/`append` if you need key-based reconciliation."
                ));
            }
            Vec::new()
        }
        MaterializeStrategy::Never => {
            if key.is_some() {
                return Err(format!(
                    "transform '{sql}': `key:` is not allowed with `materialize: never` — \
                     nothing is written, so there is no stored row to reconcile a key against. \
                     Remove `key:`, or use `materialize: upsert`/`append` if you want datamk to \
                     hold the rows."
                ));
            }
            Vec::new()
        }
        MaterializeStrategy::Append | MaterializeStrategy::Upsert => {
            let key = key.ok_or_else(|| {
                format!(
                    "transform '{sql}': missing required field `key` — a non-empty list of \
                     column identifiers, e.g. `key: [flight_id]`."
                )
            })?;
            if key.is_empty() {
                return Err(format!(
                    "transform '{sql}': `key` must be a non-empty list of column identifiers."
                ));
            }
            key
        }
    };

    Ok(TransformEntry::Materialize {
        sql,
        materialize,
        key,
    })
}

fn as_yaml_string_list(
    v: serde_yaml::Value,
    field: &str,
) -> std::result::Result<Vec<String>, String> {
    match v {
        serde_yaml::Value::Sequence(items) => items
            .into_iter()
            .map(|item| as_yaml_string(item, field))
            .collect(),
        other => Err(format!(
            "`{field}` must be a list of strings, got {}",
            yaml_kind(&other)
        )),
    }
}

/// A `transforms:` entry, validated and normalized — the engine's run loop
/// and `verify`'s grain-inheritance both dispatch on this, never the raw
/// parsed `TransformEntry` shape. Produced once, by `resolve_transforms`.
///
/// A single struct, not an enum: ADR 0008's whole point is that there is
/// **one** shape a resolved transform can have — a SELECT-only file, a
/// strategy, and (for `upsert`/`append`) a key. No raw-DML variant exists to
/// distinguish from; `TransformEntry::Path`/`::Materialize` are a *syntax*
/// choice (bare shorthand vs. explicit mapping) that both collapse to this
/// one shape here, before anything downstream ever sees them. `table` is
/// always the file's stem (no override, no exceptions) — declarative by
/// construction now that construction is the only path there is.
#[derive(Debug, Clone)]
pub struct ResolvedTransform {
    pub sql: String,
    pub strategy: MaterializeStrategy,
    /// Non-empty for `upsert`/`append`; always empty for `replace`.
    pub key: Vec<String>,
    pub table: String,
}

impl ResolvedTransform {
    /// The `table` field is public and read directly everywhere else; this
    /// wrapper exists only so a caller that just needs the file (artifact
    /// collection, the run loop's file read) doesn't have to destructure.
    pub fn file_path(&self) -> &str {
        &self.sql
    }
}

/// Whether this cell's transform layer can ever commit a snapshot. Under the
/// binding model (issue #6) a virtual export has no transform at all — it
/// points straight at `sources:` (`Export::bind`) — so "no transforms" is
/// now the ordinary shape of a cell that is entirely (or, in a mixed cell,
/// partly) bound, not a special case layered on top of `MaterializeStrategy`
/// filtering. The one place this is decided: `engine::run`'s refusal, the
/// deploy pre-flight, and `serve`'s draft-note selection all call this
/// rather than re-deriving it.
pub fn builds_no_snapshot(transforms: &[ResolvedTransform]) -> bool {
    transforms.is_empty()
}

/// Resolve-time validation for `transforms:` (ADR 0008): stem-derived table
/// naming for every entry (identifier shape, `__datamk_` rejection,
/// cross-entry collision — one naming regime, one code path, no raw special
/// case) and key identifier shape. Pure — no `${VAR}` expansion (table/key
/// names are contract, not environment) and no filesystem access beyond
/// deriving a stem from the declared path string.
///
/// Does **not** reject `materialize: never` — a `ResolvedTransform` with
/// that strategy resolves cleanly here; `config::mod::load` is the one call
/// site that rejects it afterward (`verify::check_no_materialize_never`,
/// which needs full `CellDef` context this function doesn't have — the
/// interface, to name the affected export). `execute_transform`/
/// `execute_materialize`'s `Never` match arms are `unreachable!()` on that
/// call-site discipline, not on anything this function's own type enforces
/// — `pub(crate)` (not `pub`) so every caller stays inside the module that
/// discipline actually depends on, never a hypothetical external one that
/// could skip straight to `execute_transform` with an unrejected `Never`.
pub(crate) fn resolve_transforms(transforms: &[TransformEntry]) -> Result<Vec<ResolvedTransform>> {
    let mut resolved = Vec::with_capacity(transforms.len());
    // table name -> the `sql:`/file path that claimed it, for the collision error.
    let mut claimed: IndexMap<String, String> = IndexMap::new();

    for entry in transforms {
        // The bare-path shorthand is exactly `{ sql: <path>, materialize:
        // replace }` (ADR 0008 decision 3) — normalized here, once, so
        // everything below (key validation, stem/collision checks) is a
        // single code path regardless of which syntax the author used.
        let (sql, strategy, key): (&str, MaterializeStrategy, &[String]) = match entry {
            TransformEntry::Path(path) => (path, MaterializeStrategy::Replace, &[]),
            TransformEntry::Materialize {
                sql,
                materialize,
                key,
            } => (sql, *materialize, key),
        };

        for k in key {
            if !is_valid_identifier(k) {
                anyhow::bail!(
                    "transform '{sql}': key column '{k}' is not a valid column identifier — \
                     use a bare column name matching [A-Za-z_][A-Za-z0-9_]* (no dots, quotes, \
                     or expressions)"
                );
            }
        }

        let table = file_stem(sql)?;
        claim_table(sql, &table, &mut claimed)?;

        resolved.push(ResolvedTransform {
            sql: sql.to_string(),
            strategy,
            key: key.to_vec(),
            table,
        });
    }
    Ok(resolved)
}

/// Shape-validate a stem-derived table name and register it in the
/// cross-entry collision map — every entry goes through this, since the
/// uniform naming invariant makes every entry's table name known from its
/// filename alone, checkable in one pass.
fn claim_table(sql_path: &str, table: &str, claimed: &mut IndexMap<String, String>) -> Result<()> {
    if !is_valid_identifier(table) {
        anyhow::bail!(
            "transform '{sql_path}': table name '{table}' (from the file's stem) is not a \
             valid identifier — rename the file to a valid identifier stem \
             ([A-Za-z_][A-Za-z0-9_]*)."
        );
    }
    if table.starts_with("__datamk_") {
        anyhow::bail!(
            "transform '{sql_path}': table name '{table}' uses the reserved `__datamk_` \
             prefix, which is engine-owned (watermarks and bookkeeping). Rename the file."
        );
    }
    if let Some(prev) = claimed.get(table) {
        anyhow::bail!(
            "transform '{sql_path}' and transform '{prev}' both resolve to table '{table}' — \
             every transform file must resolve to a distinct table (the uniform naming \
             invariant, ADR 0008 §2: one file, one table, named by the stem). Rename one of \
             the files."
        );
    }
    claimed.insert(table.to_string(), sql_path.to_string());
    Ok(())
}

/// The file stem of a declared transform path (`sql/fct_flights.sql` ->
/// `fct_flights`) — the table name every transform produces (the uniform
/// naming invariant, ADR 0008 §2; no override on either entry kind).
fn file_stem(sql_path: &str) -> Result<String> {
    Path::new(sql_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "transform '{sql_path}': cannot derive a table name from this path — every \
                 transform's table is its file's stem (no override exists); use a path with a \
                 valid file name"
            )
        })
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
///
/// Unknown keys are errors: a profile is a closed shape, and a key that
/// nothing reads (a typo, or a field a release removed — `discover.max_age`
/// was one) must not pass silently as if it were doing something.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Optional GCS connection. Required only when `storage` or a source is
    /// `gs://` (DuckDB's GCS reads need an HMAC pair; see `GcsBinding`).
    #[serde(default)]
    pub gcs: Option<GcsBinding>,
    /// Path to a JSON file mapping bearer token -> roles. Injected, never baked.
    /// Required only when `access.roles` is set.
    #[serde(default)]
    pub principals: Option<String>,
    /// Where rows actually live when this endpoint doesn't serve them
    /// (`serve --no-data`, ADR 0012 §4): free-form operator hints (a share
    /// name, an internal how-to URL) surfaced verbatim in the context
    /// document's `data.channels`. Environment, which is why it binds here
    /// and never in `cell.yaml`. Empty stays empty — never fabricated.
    #[serde(default)]
    pub channels: Vec<String>,
    /// Locations of upstream cell dependencies (referenced by name from `sources`).
    #[serde(default)]
    pub cells: IndexMap<String, CellLocation>,
    /// Named warehouse connections (referenced by name from `connection` sources).
    /// Environment config: the same cell reads a sandbox project in dev and the
    /// real one in prod.
    #[serde(default)]
    pub connections: IndexMap<String, Connection>,
}

// Connector config shapes live in `config::connections` (one module per
// connector, mirroring `engine::connectors`); re-exported here so `Connection`
// stays the one place every connector's config shape is enumerated.
pub use crate::config::connections::bigquery::BigQueryConnection;
pub use crate::config::connections::duckdb::DuckdbConnection;
pub use crate::config::connections::postgres::PostgresConnection;
pub use crate::config::connections::snowflake::SnowflakeConnection;

/// One named warehouse connection, tagged by `type`. A closed enum: an unknown
/// type is a parse error naming the valid types. Every field is env-expandable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Connection {
    Bigquery(BigQueryConnection),
    Postgres(PostgresConnection),
    Snowflake(SnowflakeConnection),
    Duckdb(DuckdbConnection),
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

/// GCS connection settings. Each field is env-expandable. GCS has two
/// credential planes (unlike S3's single `s3:` story): the native store
/// client (catalog publish/fetch, locks — ADR 0004) authenticates with OAuth
/// via a service-account key (`credentials`) or, when omitted, the ambient
/// ADC chain (GOOGLE_APPLICATION_CREDENTIALS, gcloud login, workload
/// identity). DuckDB's `gs://` reads take one of two paths: built-in httpfs
/// speaks only GCS's S3-interoperability API (an HMAC `key_id`/`secret`
/// pair), while `extension` swaps in a native GCS extension that uses the
/// same OAuth chain as the store — no HMAC anywhere, for orgs whose policy
/// forbids HMAC keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcsBinding {
    /// Path to a service-account key file (a path, like `principals` — never
    /// a literal token; relative paths resolve against the cell directory).
    /// Omitted = the ambient ADC chain. Drives the catalog store always, and
    /// DuckDB too when `extension` is set; built-in HMAC reads never use it.
    #[serde(default)]
    pub credentials: Option<String>,
    /// Path to a `gcs.duckdb_extension` binary (northpolesec/duckdb-gcs).
    /// When set, DuckDB reads `gs://` natively with OAuth/ADC and the HMAC
    /// pair is not needed. The build must match the vendored DuckDB version
    /// exactly (extensions are ABI-locked).
    #[serde(default)]
    pub extension: Option<String>,
    /// HMAC interoperability access key (`gcloud storage hmac create`).
    /// Required whenever DuckDB touches `gs://` and no `extension` is set.
    #[serde(default)]
    pub key_id: Option<String>,
    /// HMAC secret — pairs with `key_id`.
    #[serde(default)]
    pub secret: Option<String>,
    /// Custom endpoint host[:port] for emulators (fake-gcs-server). Applies to
    /// DuckDB's secret only; the native store client always targets real GCS.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// http vs https for the emulator endpoint. Real GCS is always https.
    #[serde(default)]
    pub use_ssl: Option<bool>,
}

impl CellDef {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading cell definition {}", path.display()))?;
        let def: CellDef = serde_yaml::from_str(&raw)
            .with_context(|| format!("parsing cell definition {}", path.display()))?;
        def.validate_prose()
            .with_context(|| format!("validating cell definition {}", path.display()))?;
        def.validate_discover()
            .with_context(|| format!("validating cell definition {}", path.display()))?;
        // ADR 0013: `docs:` paths are resolved (allowlist-by-construction:
        // relative, canonicalized, under the cell dir, never into
        // `profiles/` or `.cell/`) and cap-checked here, fail-loud — same
        // discipline as `principals:` (`load_principals`). Every caller of
        // `CellDef::load`/`config::load` gets a validated cell before
        // anything opens a connection.
        let dir = super::cell_dir(path);
        super::docs::validate_all(&dir, &def)
            .with_context(|| format!("validating cell definition {}", path.display()))?;
        Ok(def)
    }

    /// ADR 0016 §1: a discovered cell computes nothing and authors no
    /// export list, so `discover:` and any of `sources:`/`transforms:`/
    /// `interface:` together is a parse error; a `select` naming nothing is
    /// too (no "everything" default); an override must name a real semver.
    pub fn validate_discover(&self) -> Result<()> {
        let Some(d) = &self.discover else {
            return Ok(());
        };
        for (what, present) in [
            ("transforms:", !self.transforms.is_empty()),
            ("sources:", !self.sources.is_empty()),
            ("interface:", !self.interface.is_empty()),
        ] {
            if present {
                bail!(
                    "a cell with `discover:` cannot also declare `{what}` — its interface is \
                     discovered from {}'s deployed state and datamk computes nothing for it. \
                     Remove `{what}`, or remove `discover:` and author the interface by hand.",
                    match d.from {
                        DiscoverFrom::Sqlmesh => "sqlmesh",
                    }
                );
            }
        }
        if d.select.is_empty() {
            bail!(
                "`discover.select` must name at least one of `tags`, `schemas`, or `models` — \
                 there is no \"every model\" default: an interface is an explicit export list, \
                 and a project's whole DAG is not one."
            );
        }
        if d.state.trim().is_empty() || d.warehouse.trim().is_empty() {
            bail!("`discover.state` and `discover.warehouse` must each name a profile connection");
        }
        let mut seen = std::collections::HashSet::new();
        for o in &d.overrides {
            if !seen.insert(o.model.as_str()) {
                bail!("`discover.overrides` names model '{}' twice", o.model);
            }
            if let Some(v) = &o.version {
                semver::Version::parse(v).with_context(|| {
                    format!(
                        "`discover.overrides` for '{}': version '{v}' is not semver",
                        o.model
                    )
                })?;
            }
            if let Some(name) = &o.as_name {
                if !is_valid_identifier(name) {
                    bail!(
                        "`discover.overrides` for '{}': `as: {name}` is not a valid export name \
                         (letters, digits, underscores; not starting with a digit)",
                        o.model
                    );
                }
            }
        }
        Ok(())
    }

    /// ADR 0012 §3: prose is length-capped at parse time — the meaning
    /// fields are one-line-to-two-sentence orientation, not documentation
    /// pages, and the caps are also the injection-surface bound (ADR 0012
    /// §8: cell prose is the first untrusted author text to land in a
    /// trusted agent context).
    pub fn validate_prose(&self) -> Result<()> {
        const CELL_DESC_MAX: usize = 200;
        const DESC_MAX: usize = 500;
        const UNIT_MAX: usize = 16;

        if let Some(d) = &self.description {
            if d.chars().count() > CELL_DESC_MAX {
                bail!(
                    "cell `description` is {} characters (max {CELL_DESC_MAX}) — one line: \
                     what this data product is.",
                    d.chars().count()
                );
            }
            if d.contains('\n') {
                bail!("cell `description` must be a single line.");
            }
        }
        for export in &self.interface {
            if let Some(d) = &export.description {
                if d.chars().count() > DESC_MAX {
                    bail!(
                        "export '{}': `description` is {} characters (max {DESC_MAX}) — one \
                         or two sentences: what one row means.",
                        export.name,
                        d.chars().count()
                    );
                }
            }
            for (col, spec) in &export.schema {
                if let Some(u) = &spec.unit {
                    if u.chars().count() > UNIT_MAX || u.chars().any(char::is_whitespace) {
                        bail!(
                            "export '{}', column '{col}': `unit` must be a structured token \
                             like `USD` or `ms` (max {UNIT_MAX} characters, no whitespace), \
                             got '{u}' — prose belongs in `description`.",
                            export.name
                        );
                    }
                }
                if let Some(d) = &spec.description {
                    if d.chars().count() > DESC_MAX {
                        bail!(
                            "export '{}', column '{col}': `description` is {} characters \
                             (max {DESC_MAX}).",
                            export.name,
                            d.chars().count()
                        );
                    }
                }
            }
        }
        Ok(())
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
            bind: None,
            description: None,
            docs: None,
            grain: vec![],
            schema: IndexMap::new(),
            freshness: None,
            visibility: Visibility::default(),
            contract: Contract::default(),
            from: Default::default(),
            discovered: None,
        }
    }

    // --- ADR 0012 §3: the meaning fields and the two-shape schema value ---

    #[test]
    fn schema_value_bare_string_parses_as_before() {
        let def: CellDef = serde_yaml::from_str(
            "cell: c\ninterface:\n  - name: e\n    version: 1.0.0\n    schema:\n      region: string\n",
        )
        .unwrap();
        assert_eq!(
            def.interface[0].schema.get("region"),
            Some(&ColumnSpec::bare("string"))
        );
    }

    #[test]
    fn schema_value_mapping_parses_type_unit_description() {
        let def: CellDef = serde_yaml::from_str(
            "cell: c\ninterface:\n  - name: e\n    version: 1.0.0\n    schema:\n      revenue:\n        type: decimal\n        unit: USD\n        description: Gross revenue.\n",
        )
        .unwrap();
        let spec = def.interface[0].schema.get("revenue").unwrap();
        assert_eq!(spec.ty, "decimal");
        assert_eq!(spec.unit.as_deref(), Some("USD"));
        assert_eq!(spec.description.as_deref(), Some("Gross revenue."));
    }

    #[test]
    fn schema_value_typo_field_errors_naming_the_valid_fields() {
        let err = serde_yaml::from_str::<CellDef>(
            "cell: c\ninterface:\n  - name: e\n    version: 1.0.0\n    schema:\n      revenue:\n        type: decimal\n        descripton: oops\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown field `descripton`"), "got: {err}");
        assert!(err.contains("`type`"), "got: {err}");
        assert!(err.contains("`description`"), "got: {err}");
    }

    #[test]
    fn schema_value_mapping_missing_type_errors_with_the_fix() {
        let err = serde_yaml::from_str::<CellDef>(
            "cell: c\ninterface:\n  - name: e\n    version: 1.0.0\n    schema:\n      revenue:\n        unit: USD\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("missing required field `type`"), "got: {err}");
    }

    #[test]
    fn column_spec_roundtrips_bare_as_string_and_enriched_as_mapping() {
        let bare = ColumnSpec::bare("date");
        assert_eq!(serde_yaml::to_string(&bare).unwrap().trim(), "date");
        let rich = ColumnSpec {
            ty: "decimal".to_string(),
            unit: Some("USD".to_string()),
            description: None,
            from: FromMap::new(),
        };
        let yaml = serde_yaml::to_string(&rich).unwrap();
        let back: ColumnSpec = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back, rich);
    }

    fn def_with_prose(cell_desc: &str, unit: &str) -> CellDef {
        serde_yaml::from_str(&format!(
            "cell: c\ndescription: \"{cell_desc}\"\ninterface:\n  - name: e\n    version: 1.0.0\n    schema:\n      revenue:\n        type: decimal\n        unit: \"{unit}\"\n"
        ))
        .unwrap()
    }

    #[test]
    fn prose_caps_reject_an_overlong_cell_description() {
        let def = def_with_prose(&"x".repeat(201), "USD");
        let err = def.validate_prose().unwrap_err().to_string();
        assert!(err.contains("cell `description`"), "got: {err}");
        assert!(err.contains("max 200"), "got: {err}");
    }

    #[test]
    fn prose_caps_reject_a_prose_shaped_unit() {
        let def = def_with_prose("fine", "US dollars");
        let err = def.validate_prose().unwrap_err().to_string();
        assert!(
            err.contains("`unit` must be a structured token"),
            "got: {err}"
        );
        assert!(err.contains("prose belongs in `description`"), "got: {err}");
    }

    #[test]
    fn prose_caps_accept_the_intended_shapes() {
        def_with_prose("Daily orders by region.", "USD")
            .validate_prose()
            .expect("well-shaped prose must pass");
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
        assert_eq!(
            def.transforms
                .iter()
                .map(TransformEntry::file_path)
                .collect::<Vec<_>>(),
            vec!["sql/stg.sql", "sql/final.sql"]
        );

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
                query,
                incremental,
            } => {
                assert_eq!(connection, "crm");
                assert_eq!(table.as_deref(), Some("sales.accounts"));
                assert!(query.is_none());
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

    /// Low-priority fix, review-flagged: `Export` had no `deny_unknown_fields`,
    /// so `binds:` (a typo for `bind:`) would otherwise silently parse as a
    /// plain materialized export — `bind` defaults to `None`, no error
    /// anywhere near the typo, and the export reads its transform table
    /// under `name` instead of binding the intended source.
    #[test]
    fn export_unknown_field_is_rejected_not_silently_ignored() {
        let yaml = "cell: t\ninterface:\n  - name: e\n    version: 1.0.0\n    binds: raw\n";
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(err.contains("binds"), "unexpected error: {err}");
        assert!(
            err.contains("unknown field"),
            "must be rejected, not silently parsed as a plain export: {err}"
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
                "unknown field `incremenetal` — a connection source has `connection`, one of \
                 `table`/`query`, and optional `incremental`."
            ),
            "unexpected error: {err}"
        );
    }

    // --- ADR 0007: `query:` connection sources ------------------------------

    #[test]
    fn celldef_parses_a_query_connection_source() {
        let yaml = r#"
cell: orders
sources:
  raw_spend_hourly:
    connection: dw_silver
    query: |
      SELECT advertiser_id, hour, SUM(total_spend) AS total_spend
      FROM `summarydata.campaign_group_spend_by_minute`
      GROUP BY 1, 2
"#;
        let def: CellDef = serde_yaml::from_str(yaml).unwrap();
        match def.sources.get("raw_spend_hourly").unwrap() {
            Source::Connection {
                connection,
                table,
                query,
                incremental,
            } => {
                assert_eq!(connection, "dw_silver");
                assert!(table.is_none());
                let q = query.as_deref().unwrap();
                assert!(q.contains("GROUP BY 1, 2"), "got: {q}");
                assert!(incremental.is_none());
            }
            other => panic!("expected connection source, got {other:?}"),
        }
    }

    #[test]
    fn connection_source_with_both_table_and_query_errors_naming_both_fields() {
        let yaml = r#"
cell: orders
sources:
  bad:
    connection: dw_silver
    table: sales.accounts
    query: SELECT 1
"#;
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(err.contains('`') && err.contains("table"), "{err}");
        assert!(err.contains("query"), "{err}");
        assert!(
            err.contains("cannot have both `table` and `query`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn connection_source_with_neither_table_nor_query_errors_naming_both_fields() {
        let yaml = r#"
cell: orders
sources:
  bad:
    connection: dw_silver
"#;
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(err.contains("`table`"), "{err}");
        assert!(err.contains("`query`"), "{err}");
        assert!(
            err.contains("missing `table` or `query`"),
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
    staging_uri: gs://acme-bq-staging/datamk-scratch
"#;
        let b: Bindings = serde_yaml::from_str(yaml).unwrap();
        let Connection::Bigquery(bq) = b.connections.get("crm").unwrap() else {
            panic!("expected bigquery");
        };
        assert_eq!(bq.project, "acme-prod-crm");
        assert_eq!(bq.billing_project.as_deref(), Some("acme-billing"));
        assert_eq!(bq.credentials.as_deref(), Some("/etc/datamk/bq-key.json"));
        assert_eq!(
            bq.staging_uri.as_deref(),
            Some("gs://acme-bq-staging/datamk-scratch")
        );
    }

    #[test]
    fn bindings_bigquery_connection_staging_uri_is_optional() {
        let yaml = r#"
catalog: c
storage: s
connections:
  crm:
    type: bigquery
    project: acme-prod-crm
"#;
        let b: Bindings = serde_yaml::from_str(yaml).unwrap();
        let Connection::Bigquery(bq) = b.connections.get("crm").unwrap() else {
            panic!("expected bigquery");
        };
        assert_eq!(bq.staging_uri, None);
    }

    #[test]
    fn bindings_snowflake_connection_parses() {
        let yaml = r#"
catalog: c
storage: s
connections:
  wh:
    type: snowflake
    account: MYORG-ACCT
    user: SVC_USER
    database: ANALYTICS
    private_key_path: /etc/datamk/sf-key.p8
    warehouse: WH
    role: ANALYST
"#;
        let b: Bindings = serde_yaml::from_str(yaml).unwrap();
        let Connection::Snowflake(sf) = b.connections.get("wh").unwrap() else {
            panic!("expected snowflake");
        };
        assert_eq!(sf.account, "MYORG-ACCT");
        assert_eq!(sf.user.as_deref(), Some("SVC_USER"));
        assert_eq!(sf.database, "ANALYTICS");
        assert_eq!(
            sf.private_key_path.as_deref(),
            Some("/etc/datamk/sf-key.p8")
        );
        assert_eq!(sf.warehouse.as_deref(), Some("WH"));
        assert_eq!(sf.role.as_deref(), Some("ANALYST"));
        assert!(sf.authenticator.is_none());
        assert!(sf.password.is_none());
    }

    #[test]
    fn bindings_reject_an_unknown_connection_type_naming_the_valid_ones() {
        let yaml = r#"
catalog: c
storage: s
connections:
  crm:
    type: redshift
    project: p
"#;
        let err = serde_yaml::from_str::<Bindings>(yaml)
            .unwrap_err()
            .to_string();
        assert!(err.contains("redshift"), "unexpected error: {err}");
        assert!(err.contains("bigquery"), "unexpected error: {err}");
        assert!(err.contains("postgres"), "unexpected error: {err}");
        assert!(err.contains("snowflake"), "unexpected error: {err}");
    }

    #[test]
    fn bindings_postgres_connection_parses() {
        let yaml = r#"
catalog: c
storage: s
connections:
  pg:
    type: postgres
    host: db.internal.example.com
    database: analytics
    user: datamk_ro
    password: ${PG_PASSWORD}
    sslmode: verify-full
"#;
        let b: Bindings = serde_yaml::from_str(yaml).unwrap();
        let Connection::Postgres(pg) = b.connections.get("pg").unwrap() else {
            panic!("expected postgres");
        };
        assert_eq!(pg.host, "db.internal.example.com");
        assert_eq!(pg.database, "analytics");
        assert_eq!(pg.user.as_deref(), Some("datamk_ro"));
        assert_eq!(pg.password.as_deref(), Some("${PG_PASSWORD}"));
        assert_eq!(pg.sslmode.as_deref(), Some("verify-full"));
        assert!(pg.port.is_none());
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

    // --- ADR 0008: `materialize:` transform entries -------------------------

    fn transforms_yaml(entries: &str) -> Vec<TransformEntry> {
        let def: CellDef =
            serde_yaml::from_str(&format!("cell: t\ntransforms:\n{entries}")).unwrap();
        def.transforms
    }

    #[test]
    fn bare_path_transform_entry_parses() {
        let t = transforms_yaml("  - sql/stg_orders.sql");
        assert_eq!(t, vec![TransformEntry::Path("sql/stg_orders.sql".into())]);
        assert_eq!(t[0].file_path(), "sql/stg_orders.sql");
    }

    #[test]
    fn materialize_entry_parses_every_field() {
        let t = transforms_yaml(
            "  - sql: sql/fct_flights.sql\n    materialize: upsert\n    key: [flight_id]\n",
        );
        assert_eq!(
            t,
            vec![TransformEntry::Materialize {
                sql: "sql/fct_flights.sql".into(),
                materialize: MaterializeStrategy::Upsert,
                key: vec!["flight_id".into()],
            }]
        );
        assert_eq!(t[0].file_path(), "sql/fct_flights.sql");
    }

    #[test]
    fn materialize_entry_typo_field_names_the_valid_fields_not_a_generic_variant_error() {
        let yaml =
            "cell: t\ntransforms:\n  - sql: sql/fct.sql\n    materalize: upsert\n    key: [id]\n";
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field `materalize`"), "got: {err}");
        assert!(err.contains("`sql`, `materialize`, `key`."), "got: {err}");
        assert!(
            !err.contains("did not match any variant"),
            "leaked a generic untagged-enum error: {err}"
        );
    }

    #[test]
    fn materialize_entry_table_field_is_rejected_naming_the_removal_rationale() {
        // ADR 0008 §2 (founder-ratified uniform naming invariant): the
        // earlier `table:` override is gone. An author who remembers it (or
        // copies an old example) must be told why, not just "unknown
        // field".
        let yaml = "cell: t\ntransforms:\n  - sql: sql/fct.sql\n    materialize: upsert\n    key: [id]\n    table: fct_v2\n";
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field `table`"), "got: {err}");
        assert!(err.contains("no `table:` override"), "got: {err}");
        assert!(err.contains("Rename the file"), "got: {err}");
    }

    #[test]
    fn materialize_entry_missing_sql_errors() {
        let yaml = "cell: t\ntransforms:\n  - materialize: upsert\n    key: [id]\n";
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing required field `sql`"), "got: {err}");
    }

    #[test]
    fn materialize_entry_missing_materialize_errors() {
        let yaml = "cell: t\ntransforms:\n  - sql: sql/fct.sql\n    key: [id]\n";
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("transform 'sql/fct.sql': missing required field `materialize`"),
            "got: {err}"
        );
    }

    #[test]
    fn materialize_entry_missing_key_errors() {
        let yaml = "cell: t\ntransforms:\n  - sql: sql/fct.sql\n    materialize: upsert\n";
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("transform 'sql/fct.sql': missing required field `key`"),
            "got: {err}"
        );
    }

    #[test]
    fn materialize_entry_empty_key_list_errors() {
        let yaml =
            "cell: t\ntransforms:\n  - sql: sql/fct.sql\n    materialize: upsert\n    key: []\n";
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(err.contains("`key` must be a non-empty list"), "got: {err}");
    }

    #[test]
    fn materialize_entry_invalid_strategy_names_the_closed_set() {
        let yaml =
            "cell: t\ntransforms:\n  - sql: sql/fct.sql\n    materialize: merge\n    key: [id]\n";
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`materialize: merge` is not a recognized strategy"),
            "got: {err}"
        );
        assert!(
            err.contains("`append`, `upsert`, `replace`, or `never`"),
            "got: {err}"
        );
    }

    #[test]
    fn materialize_entry_replace_forbids_key() {
        let yaml =
            "cell: t\ntransforms:\n  - sql: sql/fct.sql\n    materialize: replace\n    key: [id]\n";
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`key:` is not allowed with `materialize: replace`"),
            "got: {err}"
        );
        assert!(err.contains("nothing to reconcile"), "got: {err}");
    }

    #[test]
    fn materialize_entry_replace_needs_no_key() {
        let yaml = "cell: t\ntransforms:\n  - sql: sql/fct.sql\n    materialize: replace\n";
        let def: CellDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            def.transforms,
            vec![TransformEntry::Materialize {
                sql: "sql/fct.sql".into(),
                materialize: MaterializeStrategy::Replace,
                key: vec![],
            }]
        );
    }

    // --- issue #6 (virtual cells foundation): `materialize: never` --------

    #[test]
    fn materialize_entry_never_needs_no_key() {
        let yaml = "cell: t\ntransforms:\n  - sql: sql/fct.sql\n    materialize: never\n";
        let def: CellDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            def.transforms,
            vec![TransformEntry::Materialize {
                sql: "sql/fct.sql".into(),
                materialize: MaterializeStrategy::Never,
                key: vec![],
            }]
        );
    }

    #[test]
    fn materialize_entry_never_forbids_key() {
        let yaml =
            "cell: t\ntransforms:\n  - sql: sql/fct.sql\n    materialize: never\n    key: [id]\n";
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`key:` is not allowed with `materialize: never`"),
            "got: {err}"
        );
        assert!(
            err.contains("no stored row to reconcile a key against"),
            "got: {err}"
        );
        assert!(
            err.contains("materialize: upsert`/`append` if you want datamk to hold the rows"),
            "got: {err}"
        );
    }

    #[test]
    fn materialize_entry_view_typo_names_never_specifically() {
        // The most likely typo the moment `never` ships (issue #6): `view`
        // is the issue's original proposal, rejected — the error must steer
        // the author to `never` by name, not just land them in the generic
        // closed-set message.
        let yaml = "cell: t\ntransforms:\n  - sql: sql/fct.sql\n    materialize: view\n";
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`materialize: view` is not a recognized strategy"),
            "got: {err}"
        );
        assert!(
            err.contains("datamk creates no warehouse objects"),
            "got: {err}"
        );
        assert!(
            err.contains("Use `materialize: never` to leave the rows where they are"),
            "got: {err}"
        );
    }

    #[test]
    fn materialize_entry_upsert_still_requires_key_with_replace_in_the_mix() {
        let yaml = "cell: t\ntransforms:\n  - sql: sql/fct.sql\n    materialize: upsert\n";
        let err = serde_yaml::from_str::<CellDef>(yaml)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("transform 'sql/fct.sql': missing required field `key`"),
            "got: {err}"
        );
    }

    fn materialize(sql: &str, key: &[&str]) -> TransformEntry {
        TransformEntry::Materialize {
            sql: sql.to_string(),
            materialize: MaterializeStrategy::Upsert,
            key: key.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn resolve_transforms_table_is_always_the_file_stem() {
        // No override exists (ADR 0008, "table = file stem... no
        // override") — the stem is not a default, it is the only rule.
        let entries = vec![materialize("sql/fct_flights.sql", &["flight_id"])];
        let resolved = resolve_transforms(&entries).unwrap();
        assert_eq!(resolved[0].table, "fct_flights");
    }

    #[test]
    fn resolve_transforms_bare_path_entries_default_to_replace_with_no_key() {
        // ADR 0008 decision 3: "bare path = replace: rebuild each run (the
        // default)". Both syntaxes resolve through the same code path —
        // pin that the bare path really does normalize to `Replace`/empty
        // key, not a distinct resolved shape.
        let entries = vec![TransformEntry::Path("sql/stg.sql".into())];
        let resolved = resolve_transforms(&entries).unwrap();
        assert_eq!(resolved[0].sql, "sql/stg.sql");
        assert_eq!(resolved[0].table, "stg");
        assert_eq!(resolved[0].strategy, MaterializeStrategy::Replace);
        assert!(resolved[0].key.is_empty());
    }

    #[test]
    fn resolve_transforms_an_explicit_replace_mapping_is_identical_to_the_bare_path() {
        // ADR 0008 work item / coordinator ruling: "a mapping entry with
        // materialize: replace is legal and identical to the bare path."
        let bare = resolve_transforms(&[TransformEntry::Path("sql/stg.sql".into())]).unwrap();
        let mapping = resolve_transforms(&[TransformEntry::Materialize {
            sql: "sql/stg.sql".into(),
            materialize: MaterializeStrategy::Replace,
            key: vec![],
        }])
        .unwrap();
        assert_eq!(bare[0].sql, mapping[0].sql);
        assert_eq!(bare[0].table, mapping[0].table);
        assert_eq!(bare[0].strategy, mapping[0].strategy);
        assert_eq!(bare[0].key, mapping[0].key);
    }

    #[test]
    fn resolve_transforms_rejects_a_non_identifier_file_stem_naming_the_one_fix() {
        // No `table:` override exists to offer as a second fix — renaming
        // the file is the only one.
        let entries = vec![materialize("sql/fct-flights.sql", &["flight_id"])];
        let err = resolve_transforms(&entries).unwrap_err().to_string();
        assert!(err.contains("'fct-flights'"), "got: {err}");
        assert!(err.contains("rename the file"), "got: {err}");
        assert!(!err.contains("table:"), "no override exists, got: {err}");
    }

    #[test]
    fn resolve_transforms_rejects_a_non_identifier_stem_on_a_bare_path_entry_too() {
        let entries = vec![TransformEntry::Path("sql/fct-flights.sql".into())];
        let err = resolve_transforms(&entries).unwrap_err().to_string();
        assert!(err.contains("'fct-flights'"), "got: {err}");
        assert!(err.contains("rename the file"), "got: {err}");
    }

    #[test]
    fn resolve_transforms_rejects_the_reserved_datamk_prefix() {
        // Reached via the stem now — there is no override to name it through.
        let entries = vec![materialize("sql/__datamk_fct.sql", &["flight_id"])];
        let err = resolve_transforms(&entries).unwrap_err().to_string();
        assert!(err.contains("reserved"), "got: {err}");
        assert!(err.contains("__datamk_fct"), "got: {err}");
    }

    #[test]
    fn resolve_transforms_rejects_an_invalid_key_identifier() {
        let entries = vec![materialize("sql/fct.sql", &["flight id"])];
        let err = resolve_transforms(&entries).unwrap_err().to_string();
        assert!(err.contains("key column 'flight id'"), "got: {err}");
        assert!(err.contains("not a valid column identifier"), "got: {err}");
    }

    #[test]
    fn resolve_transforms_rejects_a_cross_entry_table_collision_mapping_vs_mapping() {
        // Two files in different directories sharing a stem.
        let entries = vec![
            materialize("sql/a/shared.sql", &["id"]),
            materialize("sql/b/shared.sql", &["id"]),
        ];
        let err = resolve_transforms(&entries).unwrap_err().to_string();
        assert!(err.contains("sql/a/shared.sql"), "got: {err}");
        assert!(err.contains("sql/b/shared.sql"), "got: {err}");
        assert!(err.contains("'shared'"), "got: {err}");
        assert!(err.contains("distinct table"), "got: {err}");
    }

    #[test]
    fn resolve_transforms_rejects_a_cross_entry_table_collision_bare_path_vs_bare_path() {
        // One code path now (no raw special case) — two bare-path entries
        // with the same stem collide exactly like two mappings would.
        let entries = vec![
            TransformEntry::Path("sql/a/shared.sql".into()),
            TransformEntry::Path("sql/b/shared.sql".into()),
        ];
        let err = resolve_transforms(&entries).unwrap_err().to_string();
        assert!(err.contains("sql/a/shared.sql"), "got: {err}");
        assert!(err.contains("sql/b/shared.sql"), "got: {err}");
        assert!(err.contains("'shared'"), "got: {err}");
    }

    #[test]
    fn resolve_transforms_rejects_a_cross_entry_table_collision_bare_path_vs_mapping() {
        // The mixed-syntax case — one naming regime, checked uniformly
        // regardless of which syntax claimed the name first.
        let entries = vec![
            TransformEntry::Path("sql/a/shared.sql".into()),
            materialize("sql/b/shared.sql", &["id"]),
        ];
        let err = resolve_transforms(&entries).unwrap_err().to_string();
        assert!(err.contains("sql/a/shared.sql"), "got: {err}");
        assert!(err.contains("sql/b/shared.sql"), "got: {err}");
        assert!(err.contains("'shared'"), "got: {err}");
    }

    #[test]
    fn resolve_transforms_allows_two_entries_with_different_stems() {
        let entries = vec![
            materialize("sql/a.sql", &["id"]),
            TransformEntry::Path("sql/b.sql".into()),
        ];
        assert_eq!(resolve_transforms(&entries).unwrap().len(), 2);
    }

    #[test]
    fn is_valid_identifier_matches_the_documented_grammar() {
        assert!(is_valid_identifier("flight_id"));
        assert!(is_valid_identifier("_x"));
        assert!(!is_valid_identifier("2x"));
        assert!(!is_valid_identifier("flight-id"));
        assert!(!is_valid_identifier("flight.id"));
        assert!(!is_valid_identifier(""));
    }

    // --- binding model: `builds_no_snapshot` --------------------------------

    /// `is_all_never_true_only_when_every_transform_is_never` (pre-binding-
    /// model) asserted `!builds_no_snapshot(&[])` — under the removed
    /// `materialize: never` design, zero transforms was a rarely-meaningful
    /// edge case, deliberately excluded so it wasn't mistaken for the
    /// then-new virtual-cell feature. Under the binding model, zero
    /// transforms is the *ordinary* shape of an entirely-bound cell (every
    /// export points at `sources:` via `Export::bind`, no transform layer at
    /// all) — the old exclusion would misclassify exactly that cell as
    /// "still might build a snapshot." This is the behavior the founder's
    /// decision changes, not a regression: an empty transform list can never
    /// commit a snapshot regardless of why it's empty.
    #[test]
    fn builds_no_snapshot_true_only_when_transforms_are_empty() {
        assert!(builds_no_snapshot(&[]));
        let some = resolve_transforms(&[
            materialize("sql/a.sql", &["id"]),
            TransformEntry::Path("sql/b.sql".into()),
        ])
        .unwrap();
        assert!(!builds_no_snapshot(&some));
    }
}
