//! The mesh manifest (ADR 0012 §6): the universe of cells as a document — a
//! hint, never an authority, and never a hosted registry (a server-side index
//! of cells is a control plane; no-control-plane is the thesis; `serve` never
//! serves this). `datamk mesh emit` writes a static JSON document an operator
//! hosts anywhere — bucket, repo, intranet page.
//!
//! One owner per string: everything beyond `{name, url}` is **copied from
//! each cell's own context document by the emitter** — never typed into the
//! manifest by hand — so the manifest is a digest-stamped cache and the
//! cell's document always wins. The manifest is never a token-routing
//! authority: `auth_hint` is an opaque credential *name* an agent resolves in
//! its own secret store, never a token; token→host bindings live in the
//! agent's configuration.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The manifest-schema version, same discipline as `datamk_context`.
pub const DATAMK_MESH_VERSION: u32 = 1;

/// The mesh manifest — a closed shape: a consumer must be able to reject a
/// tampered or malformed manifest loudly, so parsing denies unknown fields.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshManifest {
    pub datamk_mesh: u32,
    pub generated_at: String,
    pub cells: Vec<MeshCell>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshCell {
    pub name: String,
    pub url: String,
    /// Copied from the cell's context document (`declared.description`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Copied from the cell's context document — what lets an agent *route*
    /// ("which cell answers revenue questions?") without N cold fetches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exports: Vec<MeshExport>,
    /// The cell's interface digest at copy time (its `/context` ETag) — the
    /// stamp that makes this entry a verifiable cache, not an authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_digest: Option<String>,
    /// An opaque credential *name* the agent resolves in its own secret
    /// store. Never a token, never a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_hint: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshExport {
    pub name: String,
    pub version: String,
    pub contract: String,
    /// Copied from the document: this export serves no rows over HTTP. Without
    /// it an agent routing off the manifest picks a bound export and gets a
    /// 404 with no warning it could have read here.
    #[serde(default)]
    pub bound: bool,
}

/// The hand-authored cells file (`mesh emit --cells`): the `{name, url}`
/// fallback for heterogeneous estates, with the anti-rot cost accepted
/// openly — which is why it carries nothing the emitter can copy instead.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CellsFile {
    cells: Vec<CellEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CellEntry {
    name: String,
    url: String,
    #[serde(default)]
    auth_hint: Option<String>,
    /// Name of an environment variable holding a bearer token the *emitter*
    /// uses to fetch this cell's context. The name of a variable, resolved
    /// at emit time — the token itself never appears in any file.
    #[serde(default)]
    bearer_env: Option<String>,
}

/// `datamk mesh emit`: build the manifest from a hand-authored cells file or
/// a store-prefix name census, fetch each cell's `/context` to copy its
/// summary, and write JSON to stdout (`--out` to write a file).
pub fn emit(
    cells_file: Option<&Path>,
    store_prefix: Option<&str>,
    url_template: Option<&str>,
    out: Option<&Path>,
) -> Result<()> {
    let entries = match (cells_file, store_prefix) {
        (Some(path), None) => load_cells_file(path)?,
        (None, Some(prefix)) => census_entries(prefix, url_template)?,
        (Some(_), Some(_)) => {
            bail!("pass either --cells or --store, not both — one source of names per manifest")
        }
        (None, None) => bail!(
            "name the cells: --cells <file> (hand-authored {{name, url}} list) or \
             --store <s3://bucket/prefix> --url-template <https://{{name}}.example> \
             (name census over a shared parent prefix)"
        ),
    };

    let mut cells = Vec::new();
    for entry in entries {
        if !entry.url.starts_with("https://") {
            tracing::warn!(
                cell = %entry.name, url = %entry.url,
                "mesh client rules are https-only (ADR 0012 §6); an http url will be \
                 rejected by conforming agents"
            );
        }
        let bearer = entry
            .bearer_env
            .as_deref()
            .and_then(|var| std::env::var(var).ok());
        let fetched = fetch_context(&entry.url, bearer.as_deref());
        cells.push(summarize(entry, fetched));
    }

    let manifest = MeshManifest {
        datamk_mesh: DATAMK_MESH_VERSION,
        generated_at: crate::timeutil::rfc3339_utc(crate::timeutil::unix_now()),
        cells,
    };
    let json = serde_json::to_string_pretty(&manifest)?;
    match out {
        Some(path) => {
            std::fs::write(path, json.as_bytes())
                .with_context(|| format!("writing {}", path.display()))?;
            eprintln!("wrote {}", path.display());
        }
        None => println!("{json}"),
    }
    Ok(())
}

fn load_cells_file(path: &Path) -> Result<Vec<CellEntry>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading cells file {}", path.display()))?;
    let file: CellsFile = serde_yaml::from_str(&raw).with_context(|| {
        format!(
            "parsing cells file {} (expected `cells: [{{name, url}}]`)",
            path.display()
        )
    })?;
    Ok(file.cells)
}

/// The store-path census (ADR 0012 §6): a **name census, not a manifest** —
/// it works only where cells share a parent prefix by convention, only on
/// object stores, and it cannot produce `url` (the store knows where data
/// lives, never where `serve` is reachable) — hence the operator-supplied
/// `--url-template`, authored config deduplicated across N cells rather
/// than eliminated.
fn census_entries(prefix: &str, url_template: Option<&str>) -> Result<Vec<CellEntry>> {
    let template = url_template.with_context(|| {
        "--store needs --url-template (e.g. \"https://{name}.data.internal\") — the store \
         knows where data lives, never where `serve` is reachable"
    })?;
    if !template.contains("{name}") {
        bail!("--url-template must contain {{name}}, got '{template}'");
    }
    let store = crate::store::Store::for_storage(prefix, None, None)?;
    let names = store
        .list_child_names()
        .with_context(|| format!("listing child prefixes under {prefix}"))?;
    if names.is_empty() {
        tracing::warn!(%prefix, "no child prefixes found — is this the shared parent prefix?");
    }
    Ok(names
        .into_iter()
        .map(|name| CellEntry {
            url: template.replace("{name}", &name),
            name,
            auth_hint: None,
            bearer_env: None,
        })
        .collect())
}

/// GET `<url>/context`. Returns the parsed document and the ETag (the
/// interface digest). Best-effort: any failure returns `None` with a
/// warning, and the manifest entry stays `{name, url}` — never fabricated.
/// The `/context` endpoint the emitter fetches — a pure helper, split out so
/// it's unit-testable without a network call. Carries no query string, ever
/// (ADR 0013 §7): the mesh emitter gets nothing from the `include=docs`
/// feature — no docs, no flag — so this asserts by construction what was
/// previously guaranteed only by nobody having typed one.
fn context_endpoint(url: &str) -> String {
    format!("{}/context", url.trim_end_matches('/'))
}

fn fetch_context(url: &str, bearer: Option<&str>) -> Option<(serde_json::Value, Option<String>)> {
    let endpoint = context_endpoint(url);
    let mut req = ureq::get(&endpoint);
    if let Some(t) = bearer {
        req = req.set("authorization", &format!("Bearer {t}"));
    }
    match req.call() {
        Ok(resp) => {
            let etag = resp.header("etag").map(|e| e.trim_matches('"').to_string());
            match resp.into_json::<serde_json::Value>() {
                Ok(doc) => Some((doc, etag)),
                Err(e) => {
                    tracing::warn!(%endpoint, error = %e, "context document unparseable; emitting name+url only");
                    None
                }
            }
        }
        Err(e) => {
            tracing::warn!(%endpoint, error = %e, "context fetch failed; emitting name+url only");
            None
        }
    }
}

/// Copy the routing summary out of a fetched context document — the only
/// writer of every field beyond `{name, url, auth_hint}` (one owner per
/// string). A fetch miss yields the bare entry, never fabricated fields.
fn summarize(entry: CellEntry, fetched: Option<(serde_json::Value, Option<String>)>) -> MeshCell {
    let mut cell = MeshCell {
        name: entry.name,
        url: entry.url,
        description: None,
        exports: Vec::new(),
        context_digest: None,
        auth_hint: entry.auth_hint,
    };
    let Some((doc, etag)) = fetched else {
        return cell;
    };
    // 1, 2, and 3 all parse here: neither the v2 rename
    // (`docs[].path` -> `source_path`) nor v3's relative request
    // affordances touch anything the emitter copies. Unknown versions
    // still bail.
    if !matches!(
        doc.get("datamk_context").and_then(|v| v.as_u64()),
        Some(1) | Some(2) | Some(3)
    ) {
        tracing::warn!(cell = %cell.name, "unrecognized datamk_context version; emitting name+url only");
        return cell;
    }
    cell.description = doc["declared"]["description"].as_str().map(str::to_string);
    if let Some(exports) = doc["declared"]["exports"].as_array() {
        cell.exports = exports
            .iter()
            .filter_map(|e| {
                Some(MeshExport {
                    name: e["name"].as_str()?.to_string(),
                    version: e["version"].as_str()?.to_string(),
                    contract: e["contract"].as_str()?.to_string(),
                    // `binding` present iff the export is bound — the same
                    // signal `query: null` carries, read positively.
                    bound: e.get("binding").is_some_and(|b| !b.is_null()),
                })
            })
            .collect();
    }
    cell.context_digest = etag;
    cell
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> CellEntry {
        CellEntry {
            name: name.to_string(),
            url: format!("https://{name}.data.internal"),
            auth_hint: Some(format!("{name}-token")),
            bearer_env: None,
        }
    }

    fn sample_context() -> serde_json::Value {
        serde_json::json!({
            "datamk_context": 1,
            "cell": "orders",
            "declared": {
                "description": "Daily order revenue by region.",
                "exports": [
                    { "name": "orders_daily", "version": "2.1.0",
                      "contract": "supported", "grain": ["order_date"] }
                ]
            }
        })
    }

    /// One owner per string (ADR 0012 §6): every field beyond
    /// `{name, url, auth_hint}` comes from the cell's own document.
    #[test]
    fn summarize_copies_the_routing_summary_from_the_document() {
        let cell = summarize(
            entry("orders"),
            Some((sample_context(), Some("digest123".to_string()))),
        );
        assert_eq!(cell.name, "orders");
        assert_eq!(
            cell.description.as_deref(),
            Some("Daily order revenue by region.")
        );
        assert_eq!(cell.exports.len(), 1);
        assert_eq!(cell.exports[0].name, "orders_daily");
        assert_eq!(cell.exports[0].contract, "supported");
        assert_eq!(cell.context_digest.as_deref(), Some("digest123"));
        assert_eq!(cell.auth_hint.as_deref(), Some("orders-token"));
    }

    /// Without this an agent routing off the manifest picks a bound export
    /// and hits a 404 the manifest could have warned it about.
    #[test]
    fn summarize_marks_a_bound_export_and_leaves_a_served_one_unmarked() {
        let mut doc = sample_context();
        doc["datamk_context"] = serde_json::json!(2);
        doc["declared"]["exports"] = serde_json::json!([
            { "name": "served", "version": "1.0.0", "contract": "supported",
              "query": { "filters": [] } },
            { "name": "bound", "version": "1.0.0", "contract": "experimental",
              "query": null,
              "binding": { "source": "gold", "object": "ds.fct_x" } },
        ]);
        let cell = summarize(entry("orders"), Some((doc, None)));
        assert_eq!(cell.exports.len(), 2);
        assert!(!cell.exports[0].bound);
        assert!(cell.exports[1].bound);
    }

    #[test]
    fn summarize_on_a_fetch_miss_keeps_the_bare_entry_and_fabricates_nothing() {
        let cell = summarize(entry("orders"), None);
        assert_eq!(cell.name, "orders");
        assert!(cell.description.is_none());
        assert!(cell.exports.is_empty());
        assert!(cell.context_digest.is_none());
    }

    /// ADR 0014's `datamk_context: 3` (relative request affordances) copies
    /// nothing new into the manifest — this must not silently regress into
    /// the unknown-version branch.
    #[test]
    fn summarize_accepts_context_version_3() {
        let mut doc = sample_context();
        doc["datamk_context"] = serde_json::json!(3);
        let cell = summarize(entry("orders"), Some((doc, Some("d".into()))));
        assert_eq!(
            cell.description.as_deref(),
            Some("Daily order revenue by region.")
        );
        assert_eq!(cell.exports.len(), 1);
    }

    #[test]
    fn summarize_refuses_an_unknown_document_version() {
        let mut doc = sample_context();
        doc["datamk_context"] = serde_json::json!(99);
        let cell = summarize(entry("orders"), Some((doc, Some("d".into()))));
        assert!(
            cell.description.is_none(),
            "unknown version must not be copied"
        );
        assert!(cell.exports.is_empty());
    }

    /// ADR 0013 §7: the mesh emitter gets nothing from the docs feature — no
    /// docs, no flag — so its fetch URL must never carry a query string.
    #[test]
    fn context_endpoint_never_carries_a_query_string() {
        assert_eq!(
            context_endpoint("https://orders.data.internal"),
            "https://orders.data.internal/context"
        );
        assert_eq!(
            context_endpoint("https://orders.data.internal/"),
            "https://orders.data.internal/context",
            "a trailing slash must not double up"
        );
        for url in ["https://a.example", "https://b.example/"] {
            assert!(!context_endpoint(url).contains('?'), "{url}");
        }
    }

    /// The manifest is a closed shape — a consumer rejects unknown fields
    /// loudly instead of silently carrying attacker-added ones.
    #[test]
    fn manifest_parse_denies_unknown_fields() {
        let ok = r#"{ "datamk_mesh": 1, "generated_at": "t", "cells": [
            { "name": "a", "url": "https://a" } ] }"#;
        serde_json::from_str::<MeshManifest>(ok).expect("closed shape parses");
        let bad = r#"{ "datamk_mesh": 1, "generated_at": "t", "cells": [],
                       "registry": "https://evil" }"#;
        assert!(serde_json::from_str::<MeshManifest>(bad).is_err());
        let bad_cell = r#"{ "datamk_mesh": 1, "generated_at": "t", "cells": [
            { "name": "a", "url": "https://a", "token": "secret" } ] }"#;
        assert!(serde_json::from_str::<MeshManifest>(bad_cell).is_err());
    }

    #[test]
    fn cells_file_parses_and_denies_unknown_fields() {
        let ok = "cells:\n  - name: orders\n    url: https://orders.internal\n    auth_hint: orders-token\n";
        let f: CellsFile = serde_yaml::from_str(ok).unwrap();
        assert_eq!(f.cells[0].name, "orders");
        let bad = "cells:\n  - name: orders\n    url: https://o\n    token: nope\n";
        assert!(serde_yaml::from_str::<CellsFile>(bad).is_err());
    }
}
