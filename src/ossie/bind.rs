//! Binding Ossie datasets to exports (ADR 0018 §5): the seam between the
//! ingested `semantic_model.json` record and the materialized interface. A
//! dataset binds to an export when its `source`, normalized, equals the
//! export's discovered model name, its route key, or its name. Built once,
//! at `config::load`, after discovery materializes `def.interface` — the
//! same placement ADR 0016/0017 use for `applies_to`.

use std::collections::BTreeMap;

use super::record::{Resolved, SemanticModelRecord};
use super::{Dataset, SemanticModel};
use crate::config::Export;

/// One cell's semantic model, bound to its exports. Everything a later
/// consumer (verify, and eventually context/serve) needs, without touching
/// `.cell/semantic_model.json` again: the routes each dataset is bound to,
/// and the record's own provenance fields.
#[derive(Debug, Clone)]
pub struct SemanticIndex {
    record: SemanticModelRecord,
    /// (model name, dataset name) -> the routes it's bound to, in
    /// `def.interface` declaration order. Empty/absent means unbound — the
    /// dataset is still kept in `record.document`, just excluded from every
    /// route-scoped lookup (ADR 0018 §5).
    bindings: BTreeMap<(String, String), Vec<String>>,
}

impl SemanticIndex {
    /// Bind every dataset of `record` against `interface`. Pure — no I/O,
    /// no DB; `config::load` is the only real caller, and tests construct
    /// this directly against a hand-built record.
    pub fn build(record: SemanticModelRecord, interface: &[Export]) -> SemanticIndex {
        let mut bindings: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
        for model in &record.document.semantic_model {
            for ds in &model.datasets {
                let normalized_source = normalize_source(&ds.source);
                let mut routes = Vec::new();
                for export in interface {
                    let Ok(route) = export.route() else {
                        continue; // an invalid semver has already failed load elsewhere.
                    };
                    let matches = export
                        .discovered
                        .as_ref()
                        .map(|d| normalize_source(&d.model) == normalized_source)
                        .unwrap_or(false)
                        || normalize_source(&route) == normalized_source
                        || normalize_source(&export.name) == normalized_source;
                    if matches {
                        routes.push(route);
                    }
                }
                if !routes.is_empty() {
                    bindings.insert((model.name.clone(), ds.name.clone()), routes);
                }
            }
        }
        SemanticIndex { record, bindings }
    }

    /// Every `(model name, dataset)` bound to `route`, in model/dataset
    /// declaration order.
    pub fn datasets_for_route(&self, route: &str) -> Vec<(&str, &Dataset)> {
        let mut out = Vec::new();
        for model in &self.record.document.semantic_model {
            for ds in &model.datasets {
                let key = (model.name.clone(), ds.name.clone());
                if self
                    .bindings
                    .get(&key)
                    .is_some_and(|routes| routes.iter().any(|r| r == route))
                {
                    out.push((model.name.as_str(), ds));
                }
            }
        }
        out
    }

    /// The routes `(model, dataset)` is bound to — empty when unbound.
    pub fn route_for(&self, model: &str, dataset: &str) -> &[String] {
        self.bindings
            .get(&(model.to_string(), dataset.to_string()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Not read by `verify` (which iterates `models()`) — a lookup phase 3's
    /// `?model=<name>` door needs (ADR 0018 §7); kept here rather than
    /// invented there so both consumers share the same binding.
    #[allow(dead_code)]
    pub fn model(&self, name: &str) -> Option<&SemanticModel> {
        self.record
            .document
            .semantic_model
            .iter()
            .find(|m| m.name == name)
    }

    pub fn models(&self) -> &[SemanticModel] {
        &self.record.document.semantic_model
    }

    pub fn synced_at(&self) -> &str {
        &self.record.synced_at
    }

    pub fn content_sha256(&self) -> &str {
        &self.record.content_sha256
    }

    /// Unread today — phase 3's `/context` `semantic_models[]` summary (ADR
    /// 0018 §7) surfaces the source's resolved commit/dir.
    #[allow(dead_code)]
    pub fn resolved(&self) -> &Resolved {
        &self.record.resolved
    }

    pub fn files(&self) -> &[String] {
        &self.record.files
    }

    /// Unread today — phase 3's `semantic_matches[]` (ADR 0018 §7) names
    /// which file a model/dataset/field came from.
    #[allow(dead_code)]
    pub fn model_files(&self) -> &indexmap::IndexMap<String, String> {
        &self.record.model_files
    }
}

/// ADR 0018 §5: split `source` on `.`, respecting quotes (a dot inside a
/// `"..."` or `` `...` `` span never splits), strip one layer of surrounding
/// `"`/`` ` `` per part, ASCII-lowercase, rejoin with `.`. Applied
/// identically to a dataset's `source`, an export's discovered model name,
/// its route key, and its name, so the four are compared on equal footing.
pub fn normalize_source(s: &str) -> String {
    split_respecting_quotes(s.trim())
        .into_iter()
        .map(|part| strip_quotes(part.trim()).to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(".")
}

fn split_respecting_quotes(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut quote: Option<char> = None;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '`' => quote = Some(c),
            None if c == '.' => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            None => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

fn strip_quotes(part: &str) -> &str {
    for q in ['"', '`'] {
        if part.len() >= 2 && part.starts_with(q) && part.ends_with(q) {
            return &part[1..part.len() - 1];
        }
    }
    part
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ossie::source::SemanticModelSource;
    use crate::ossie::{Dataset as OssieDataset, Document, SemanticModel as OssieModel};

    #[test]
    fn normalize_strips_quotes_lowercases_and_respects_quoted_dots() {
        assert_eq!(
            normalize_source(r#""dw-main-silver".invoice.flight_spend"#),
            "dw-main-silver.invoice.flight_spend"
        );
        assert_eq!(normalize_source("FLIGHT_SPEND@1"), "flight_spend@1");
        assert_eq!(normalize_source("`My Table`"), "my table");
    }

    fn record(models: Vec<OssieModel>) -> SemanticModelRecord {
        SemanticModelRecord {
            datamk_version: "0.0.0".to_string(),
            cell_yaml_digest: "d".to_string(),
            synced_at: "2026-09-10T00:00:00Z".to_string(),
            source: SemanticModelSource::Dir {
                dir: "osi".to_string(),
            },
            resolved: Resolved::Dir {
                dir: "/abs/osi".to_string(),
            },
            content_sha256: "abc".to_string(),
            files: vec!["m.yaml".to_string()],
            model_files: indexmap::IndexMap::new(),
            document: Document {
                version: "0.1.1".to_string(),
                dialects: vec![],
                vendors: vec![],
                semantic_model: models,
            },
        }
    }

    fn dataset(name: &str, source: &str) -> OssieDataset {
        OssieDataset {
            name: name.to_string(),
            source: source.to_string(),
            primary_key: vec![],
            unique_keys: vec![],
            description: None,
            ai_context: None,
            fields: vec![],
            custom_extensions: vec![],
        }
    }

    fn model(name: &str, datasets: Vec<OssieDataset>) -> OssieModel {
        OssieModel {
            name: name.to_string(),
            description: None,
            ai_context: None,
            datasets,
            relationships: vec![],
            metrics: vec![],
            custom_extensions: vec![],
        }
    }

    fn export(name: &str, version: &str) -> Export {
        serde_yaml::from_str(&format!("name: {name}\nversion: {version}\n")).unwrap()
    }

    #[test]
    fn exposes_the_records_own_provenance() {
        let mut rec = record(vec![model(
            "invoice",
            vec![dataset("flight_spend", "flight_spend")],
        )]);
        rec.model_files
            .insert("invoice".to_string(), "invoice.yaml".to_string());
        let idx = SemanticIndex::build(rec, &[export("flight_spend", "1.0.0")]);
        assert_eq!(idx.synced_at(), "2026-09-10T00:00:00Z");
        assert_eq!(idx.content_sha256(), "abc");
        assert_eq!(idx.files(), ["m.yaml"]);
        assert_eq!(idx.model_files()["invoice"], "invoice.yaml");
        match idx.resolved() {
            Resolved::Dir { dir } => assert_eq!(dir, "/abs/osi"),
            Resolved::Commit { .. } => panic!("expected a dir source"),
        }
        assert_eq!(idx.model("invoice").unwrap().name, "invoice");
        assert!(idx.model("nonexistent").is_none());
    }

    #[test]
    fn binds_by_route() {
        let rec = record(vec![model(
            "invoice",
            vec![dataset("flight_spend", "flight_spend@1")],
        )]);
        let idx = SemanticIndex::build(rec, &[export("flight_spend", "1.0.0")]);
        assert_eq!(idx.route_for("invoice", "flight_spend"), ["flight_spend@1"]);
        assert_eq!(
            idx.datasets_for_route("flight_spend@1")
                .iter()
                .map(|(m, d)| (*m, d.name.as_str()))
                .collect::<Vec<_>>(),
            vec![("invoice", "flight_spend")]
        );
    }

    #[test]
    fn binds_by_name() {
        let rec = record(vec![model(
            "invoice",
            vec![dataset("flight_spend", "flight_spend")],
        )]);
        let idx = SemanticIndex::build(rec, &[export("flight_spend", "1.0.0")]);
        assert_eq!(idx.route_for("invoice", "flight_spend"), ["flight_spend@1"]);
    }

    #[test]
    fn binds_by_quoted_discovered_fqn() {
        let rec = record(vec![model(
            "invoice",
            vec![dataset("flight_spend", r#""dw"."invoice"."flight_spend""#)],
        )]);
        let mut exp = export("flight_spend", "1.0.0");
        exp.discovered = Some(crate::config::DiscoveredExport {
            model: r#""dw"."invoice"."flight_spend""#.to_string(),
            kind: "table".to_string(),
            cron: None,
            owner: None,
            tags: vec![],
            fingerprint: "f".to_string(),
            version: None,
            data_hash: None,
            intervals: None,
            pending_restatement: None,
            depends_on: vec![],
            depends_on_unselected: 0,
            at: "2026-09-10T00:00:00Z".to_string(),
        });
        let idx = SemanticIndex::build(rec, &[exp]);
        assert_eq!(idx.route_for("invoice", "flight_spend"), ["flight_spend@1"]);
    }

    #[test]
    fn unbound_dataset_binds_to_nothing() {
        let rec = record(vec![model(
            "invoice",
            vec![dataset("advertisers", "advertisers")],
        )]);
        let idx = SemanticIndex::build(rec, &[export("flight_spend", "1.0.0")]);
        assert!(idx.route_for("invoice", "advertisers").is_empty());
        assert!(idx.datasets_for_route("flight_spend@1").is_empty());
        // Still present in the full model list — unbound, never dropped.
        assert_eq!(idx.model("invoice").unwrap().datasets.len(), 1);
    }

    #[test]
    fn binds_to_two_majors_of_one_name() {
        let rec = record(vec![model(
            "invoice",
            vec![dataset("flight_spend", "flight_spend")],
        )]);
        let idx = SemanticIndex::build(
            rec,
            &[
                export("flight_spend", "1.0.0"),
                export("flight_spend", "2.0.0"),
            ],
        );
        let routes = idx.route_for("invoice", "flight_spend");
        assert_eq!(routes, ["flight_spend@1", "flight_spend@2"]);
    }
}
