//! Apache Ossie ingest (ADR 0018): datamake never authors semantic
//! vocabulary — it ingests Ossie documents named by `semantic_model:`
//! (`source`), snapshots them (`record`), and (a later phase) verifies
//! every claim it can against the built tables without executing anything.
//!
//! This module is the hand-written, strict reader for one Ossie document
//! (ADR 0018 §2 point 3): every struct denies unknown fields except
//! `AiContext`'s object form, which the spec itself declares
//! `additionalProperties: true`. Not generated from `ossie-schema.json` —
//! codegen would emit permissive types wherever the spec is permissive
//! without datamake choosing to (ADR 0018, "Refused").
//!
//! `Dialect` is the union of the 0.1.1 and 0.2.0.dev0 enumerations (0.1.1's
//! six are a subset of 0.2's nine); `DataType` exists only in 0.2 and is
//! simply absent (`None`) on a 0.1.1 document. One struct set reads both
//! supported versions rather than one per version.

pub mod bind;
pub mod git;
pub mod record;
pub mod source;
pub mod verify;

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};

/// Ossie versions datamake ingests (ADR 0018 §2 point 3). Every file in one
/// `semantic_model:` source must declare one of these, and must all agree
/// (`source::merge`).
pub const SUPPORTED_VERSIONS: [&str; 2] = ["0.1.1", "0.2.0.dev0"];

/// One Ossie document, exactly the `required: [version, semantic_model]`
/// top-level shape (ADR 0018 §2 point 2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dialects: Vec<Dialect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vendors: Vec<String>,
    pub semantic_model: Vec<SemanticModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticModel {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_context: Option<AiContext>,
    pub datasets: Vec<Dataset>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relationships: Vec<Relationship>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metrics: Vec<Metric>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_extensions: Vec<CustomExtension>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dataset {
    pub name: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub primary_key: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unique_keys: Vec<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_context: Option<AiContext>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<Field>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_extensions: Vec<CustomExtension>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub name: String,
    pub expression: Expression,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<Dimension>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datatype: Option<DataType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_context: Option<AiContext>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_extensions: Vec<CustomExtension>,
}

/// `dialects` is required non-empty (spec `minItems: 1`) — checked by
/// `validate_shape`, not the type itself: a `Vec` has no minimum-length
/// encoding in serde alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expression {
    pub dialects: Vec<DialectExpression>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialectExpression {
    pub dialect: Dialect,
    pub expression: String,
}

/// The union of the 0.1.1 and 0.2.0.dev0 `Dialect` enumerations — 0.1.1's
/// six values are a strict subset of 0.2's nine, so one closed set reads
/// both without a per-version branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dialect {
    #[serde(rename = "ANSI_SQL")]
    AnsiSql,
    #[serde(rename = "SNOWFLAKE")]
    Snowflake,
    #[serde(rename = "MDX")]
    Mdx,
    #[serde(rename = "TABLEAU")]
    Tableau,
    #[serde(rename = "DATABRICKS")]
    Databricks,
    #[serde(rename = "MAQL")]
    Maql,
    #[serde(rename = "BIGQUERY")]
    Bigquery,
    #[serde(rename = "SIGMA")]
    Sigma,
    #[serde(rename = "THOUGHTSPOT")]
    Thoughtspot,
}

impl Dialect {
    /// The wire token, matching each variant's `#[serde(rename)]` exactly —
    /// the context document's `fields[].dialects` (ADR 0018 §7) needs this
    /// as a plain `&str` without a serde round-trip through `serde_json`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Dialect::AnsiSql => "ANSI_SQL",
            Dialect::Snowflake => "SNOWFLAKE",
            Dialect::Mdx => "MDX",
            Dialect::Tableau => "TABLEAU",
            Dialect::Databricks => "DATABRICKS",
            Dialect::Maql => "MAQL",
            Dialect::Bigquery => "BIGQUERY",
            Dialect::Sigma => "SIGMA",
            Dialect::Thoughtspot => "THOUGHTSPOT",
        }
    }
}

/// 0.2.0.dev0 only; simply absent on a 0.1.1 document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataType {
    String,
    Integer,
    Decimal,
    Float,
    Boolean,
    Date,
    Time,
    DateTime,
    DateTimeTz,
    Opaque,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dimension {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_time: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relationship {
    pub name: String,
    pub from: String,
    pub to: String,
    pub from_columns: Vec<String>,
    pub to_columns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_context: Option<AiContext>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_extensions: Vec<CustomExtension>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metric {
    pub name: String,
    pub expression: Expression,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datatype: Option<DataType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_context: Option<AiContext>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_extensions: Vec<CustomExtension>,
}

/// `custom_extensions[].data` is a JSON string, passed through
/// byte-for-byte here — a later phase parses it for `vendor_name ==
/// "DATAMAKE"` only (ADR 0018 §2 point 3); every other vendor's payload is
/// opaque to datamake by design.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomExtension {
    pub vendor_name: String,
    pub data: String,
}

/// `oneOf` a plain string or an object with `additionalProperties: true`
/// (ADR 0018 §2 point 3) — dispatch on YAML shape, hand-rolled rather than
/// `#[serde(untagged)]` on the whole enum, so a shape that is neither gets
/// a message naming what was expected instead of serde's generic "data did
/// not match any variant" (the `DefinitionsSource` pattern,
/// `config::schema`).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum AiContext {
    Prose(String),
    Detail(AiContextDetail),
}

/// The object form of `ai_context`. Unlike every other struct in this
/// module, this one does NOT `deny_unknown_fields`: the spec declares
/// `additionalProperties: true` here on purpose, so an unrecognized key is
/// carried uninterpreted in `extra` rather than rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiContextDetail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub synonyms: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl<'de> Deserialize<'de> for AiContext {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let value = serde_yaml::Value::deserialize(deserializer)?;
        match value {
            serde_yaml::Value::String(s) => Ok(AiContext::Prose(s)),
            serde_yaml::Value::Mapping(_) => {
                let detail: AiContextDetail =
                    serde_yaml::from_value(value).map_err(D::Error::custom)?;
                Ok(AiContext::Detail(detail))
            }
            other => Err(D::Error::custom(format!(
                "`ai_context` must be a string or an object ({{instructions, synonyms, \
                 examples, ...}}), got {}",
                yaml_kind(&other)
            ))),
        }
    }
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

impl Document {
    /// Parse one Ossie document (ADR 0018 §2), detecting exactly four
    /// cases before ever attempting the strict typed deserialize:
    ///
    /// 1. Not parseable YAML/JSON at all.
    /// 2. A 0.2 ontology document (`ontology:`, no `semantic_model:`).
    /// 3. Neither `version` nor `semantic_model` present — names the
    ///    top-level keys the file did have, so `semantic_models:` (plural)
    ///    is obvious.
    /// 4. `version` present but not one of `SUPPORTED_VERSIONS`.
    ///
    /// `file_label` is the relative path used in every message this
    /// returns.
    pub fn parse_str(text: &str, file_label: &str) -> Result<Document> {
        let value: serde_yaml::Value = serde_yaml::from_str(text)
            .with_context(|| format!("{file_label} is not a parseable YAML/JSON document"))?;

        let mapping = match &value {
            serde_yaml::Value::Mapping(m) => Some(m),
            _ => None,
        };
        let has_key = |k: &str| mapping.is_some_and(|m| m.contains_key(k));

        if has_key("ontology") && !has_key("semantic_model") {
            bail!(
                "{file_label} is an Apache Ossie ontology document (`ontology:`), not a \
                 semantic model document — datamk ingests `semantic_model:` documents; the \
                 ontology document is not one."
            );
        }
        if !has_key("version") || !has_key("semantic_model") {
            let keys = mapping
                .map(|m| {
                    m.keys()
                        .filter_map(|k| k.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            bail!(
                "{file_label} is not an Ossie semantic model document — expected top-level \
                 `version` and `semantic_model` keys, found: {}",
                if keys.is_empty() {
                    "(none — the document is not a map)".to_string()
                } else {
                    keys
                }
            );
        }

        let version = mapping
            .and_then(|m| m.get("version"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if !SUPPORTED_VERSIONS.contains(&version.as_str()) {
            bail!(
                "{file_label} declares `version: {version}` — datamk ingests Apache Ossie {} \
                 only.",
                SUPPORTED_VERSIONS.join(" or ")
            );
        }

        let doc: Document = serde_yaml::from_value(value)
            .with_context(|| format!("parsing {file_label} as an Ossie semantic model document"))?;
        validate_shape(&doc, file_label)?;
        Ok(doc)
    }
}

/// `minItems: 1` constraints the spec places on arrays a plain `Vec` can't
/// encode by itself: every field's and metric's `expression.dialects`, a
/// relationship's `from_columns`/`to_columns`, and a model's `datasets`.
fn validate_shape(doc: &Document, file_label: &str) -> Result<()> {
    for m in &doc.semantic_model {
        if m.datasets.is_empty() {
            bail!(
                "{file_label}: semantic model '{}' declares no datasets — `datasets` requires \
                 at least one.",
                m.name
            );
        }
        for ds in &m.datasets {
            for f in &ds.fields {
                if f.expression.dialects.is_empty() {
                    bail!(
                        "{file_label}: model '{}' dataset '{}' field '{}': `expression.dialects` \
                         requires at least one entry.",
                        m.name,
                        ds.name,
                        f.name
                    );
                }
            }
        }
        for met in &m.metrics {
            if met.expression.dialects.is_empty() {
                bail!(
                    "{file_label}: model '{}' metric '{}': `expression.dialects` requires at \
                     least one entry.",
                    m.name,
                    met.name
                );
            }
        }
        for r in &m.relationships {
            if r.from_columns.is_empty() || r.to_columns.is_empty() {
                bail!(
                    "{file_label}: model '{}' relationship '{}': `from_columns` and \
                     `to_columns` each require at least one entry.",
                    m.name,
                    r.name
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_02(name: &str) -> String {
        format!(
            r#"version: 0.2.0.dev0
semantic_model:
  - name: {name}
    datasets:
      - name: invoice
        source: '"dw"."public"."invoice"'
        fields:
          - name: id
            expression:
              dialects:
                - dialect: ANSI_SQL
                  expression: id
            datatype: Integer
"#
        )
    }

    fn minimal_011(name: &str) -> String {
        format!(
            r#"version: 0.1.1
semantic_model:
  - name: {name}
    datasets:
      - name: invoice
        source: '"dw"."public"."invoice"'
        fields:
          - name: id
            expression:
              dialects:
                - dialect: ANSI_SQL
                  expression: id
"#
        )
    }

    #[test]
    fn parses_0_1_1_with_no_datatype() {
        let doc = Document::parse_str(&minimal_011("m"), "f.yaml").unwrap();
        assert_eq!(doc.version, "0.1.1");
        assert!(doc.semantic_model[0].datasets[0].fields[0]
            .datatype
            .is_none());
    }

    #[test]
    fn parses_0_2_0_dev0_with_datatype() {
        let doc = Document::parse_str(&minimal_02("m"), "f.yaml").unwrap();
        assert_eq!(doc.version, "0.2.0.dev0");
        assert_eq!(
            doc.semantic_model[0].datasets[0].fields[0].datatype,
            Some(DataType::Integer)
        );
    }

    #[test]
    fn rejects_unparseable_yaml() {
        let err = Document::parse_str("not: [valid: yaml", "bad.yaml").unwrap_err();
        assert!(err
            .to_string()
            .contains("not a parseable YAML/JSON document"));
    }

    #[test]
    fn rejects_ontology_document() {
        let text = "ontology:\n  entities: []\n";
        let err = Document::parse_str(text, "ontology.yaml").unwrap_err();
        assert!(err.to_string().contains("ontology document"));
    }

    #[test]
    fn rejects_neither_key_and_names_top_level_keys() {
        let text = "semantic_models:\n  - name: oops\n";
        let err = Document::parse_str(text, "typo.yaml").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not an Ossie semantic model document"));
        assert!(msg.contains("semantic_models"));
    }

    #[test]
    fn rejects_unsupported_version() {
        let text = "version: 9.9.9\nsemantic_model: []\n";
        let err = Document::parse_str(text, "future.yaml").unwrap_err();
        assert!(err.to_string().contains("version: 9.9.9"));
    }

    #[test]
    fn ai_context_string_form() {
        let text = "version: 0.1.1\nsemantic_model:\n  - name: m\n    ai_context: plain prose\n    datasets:\n      - name: d\n        source: s\n";
        let doc = Document::parse_str(text, "f.yaml").unwrap();
        match doc.semantic_model[0].ai_context.as_ref().unwrap() {
            AiContext::Prose(s) => assert_eq!(s, "plain prose"),
            AiContext::Detail(_) => panic!("expected string form"),
        }
    }

    #[test]
    fn ai_context_object_form_with_extra_keys() {
        let text = "version: 0.1.1\nsemantic_model:\n  - name: m\n    ai_context:\n      instructions: use carefully\n      synonyms: [rev, revenue]\n      vendor_hint: acme\n    datasets:\n      - name: d\n        source: s\n";
        let doc = Document::parse_str(text, "f.yaml").unwrap();
        match doc.semantic_model[0].ai_context.as_ref().unwrap() {
            AiContext::Detail(d) => {
                assert_eq!(d.instructions.as_deref(), Some("use carefully"));
                assert_eq!(d.synonyms, vec!["rev".to_string(), "revenue".to_string()]);
                assert_eq!(
                    d.extra.get("vendor_hint").and_then(|v| v.as_str()),
                    Some("acme")
                );
            }
            AiContext::Prose(_) => panic!("expected object form"),
        }
    }

    #[test]
    fn deny_unknown_field_on_field() {
        let text = "version: 0.1.1\nsemantic_model:\n  - name: m\n    datasets:\n      - name: d\n        source: s\n        fields:\n          - name: id\n            expression:\n              dialects:\n                - dialect: ANSI_SQL\n                  expression: id\n            typo_field: oops\n";
        let err = Document::parse_str(text, "f.yaml").unwrap_err();
        assert!(
            err.to_string().contains("typo_field") || format!("{err:#}").contains("typo_field")
        );
    }

    #[test]
    fn empty_expression_dialects_is_refused() {
        let text = "version: 0.1.1\nsemantic_model:\n  - name: m\n    datasets:\n      - name: d\n        source: s\n        fields:\n          - name: id\n            expression:\n              dialects: []\n";
        let err = Document::parse_str(text, "f.yaml").unwrap_err();
        assert!(err.to_string().contains("expression.dialects"));
    }
}
