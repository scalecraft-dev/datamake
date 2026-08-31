use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::config::CellDef;

/// One file delivered into the base image at deploy time. `rel_path` (relative to
/// the cell dir) doubles as the mount-relative key a target uses (e.g. a
/// ConfigMap key in ADR 0002).
#[derive(Debug, Clone)]
pub struct ArtifactFile {
    pub rel_path: String,
    pub bytes: Vec<u8>,
}

/// The non-secret deliverable content of a cell: its definition, transforms, and
/// the release pin. The **profile is deliberately excluded** — it is secret-grade
/// and travels separately as a target-native secret, referenced not embedded.
///
/// `collect` is pure I/O: no DuckDB, no `resolve`. It never opens a database.
#[derive(Debug, Clone)]
pub struct CellArtifact {
    /// Source directory this artifact was collected from. Not read by pure
    /// rendering (ADR 0002 step 2 keeps local paths out of manifests), kept for
    /// diagnostics and possible future targets (e.g. the deferred init-container
    /// pull model, ADR 0002 "Alternatives considered").
    #[allow(dead_code)]
    pub dir: PathBuf,
    pub cell_yaml: ArtifactFile,
    pub sql: Vec<ArtifactFile>,
    /// Docs pages (ADR 0013 §9): the cell-level page (if declared) plus every
    /// export's, deduplicated by relative path — the same file referenced by
    /// both `cell.yaml`'s `docs:` and an export's collects once, so
    /// `render_configmap`'s duplicate-key guard never sees a false collision
    /// on identical content. Without this, a deploy that changes only prose
    /// would leave `content_hash` unchanged, the ConfigMap name would not
    /// change, and the workload would never roll — serving stale prose
    /// forever.
    pub docs: Vec<ArtifactFile>,
    /// `.cell/published.json` if a release pin exists. The pin travels **with**
    /// the content so a deployed Server serves supported routes at their frozen
    /// snapshot rather than silently downgrading them to latest.
    pub published: Option<ArtifactFile>,
    /// `.cell/source_check.json` if a live-verify record exists (issue #16)
    /// — same "travels with the content" reasoning as `published`, so a
    /// deployed Server can read it at startup (`serve`'s `SourceCheckRecord
    /// ::fresh_for`) instead of it being a build artifact the deploy never
    /// carries. Folded into `content_hash` below: a fresh `datamk verify`
    /// (a new `checked_at`) must roll the workload the same way a fresh
    /// release does, since a new attestation is new served content.
    pub source_check: Option<ArtifactFile>,
    /// `.cell/source_descriptions.json` if a live-verify record exists
    /// (issue #6/#10) — sibling of `source_check` immediately above, same
    /// "travels with the content" reasoning, same fold into `content_hash`.
    pub source_descriptions: Option<ArtifactFile>,
    /// `.cell/deployed_catalog.json` if a `datamk sync` record exists (ADR
    /// 0016 §5) — the whole interface of a discovered cell; travels and
    /// rolls exactly like `source_check`.
    pub deployed_catalog: Option<ArtifactFile>,
    /// SHA-256 over (cell_yaml ++ sql ++ docs ++ published ++ source_check
    /// ++ source_descriptions ++ deployed_catalog), each entry framed by its `rel_path`. A
    /// stable content identity: re-releasing (a new pin), a new live-verify
    /// record, or editing a docs page changes it, which a target uses to
    /// roll the workload (ADR 0002, ADR 0013 §9, issue #16, issue #10).
    pub content_hash: String,
}

impl CellArtifact {
    /// Gather a cell's deliverable bytes off disk. `cell_yaml_rel` is the cell
    /// definition's path relative to `dir` (normally `cell.yaml`).
    pub fn collect(dir: &Path, cell_yaml_rel: &str, def: &CellDef) -> Result<Self> {
        let cell_yaml = read_artifact(dir, cell_yaml_rel)?;

        let mut sql = Vec::with_capacity(def.transforms.len());
        for t in &def.transforms {
            sql.push(read_artifact(dir, t.file_path())?);
        }

        // ADR 0013 §9 / ADR 0017 §4: every declared `docs:` path — cell-
        // level, every export's, the `definitions:` file (when authored as
        // one), and every definition's own `docs:` page — deduplicated by
        // relative path before reading, so the same file named by two
        // fields collects once.
        let mut docs_paths: Vec<&str> = Vec::new();
        if let Some(p) = &def.docs {
            docs_paths.push(p.as_str());
        }
        for export in &def.interface {
            if let Some(p) = &export.docs {
                if !docs_paths.contains(&p.as_str()) {
                    docs_paths.push(p.as_str());
                }
            }
        }
        if let Some(p) = &def.definitions_file {
            if !docs_paths.contains(&p.as_str()) {
                docs_paths.push(p.as_str());
            }
        }
        for d in &def.definitions {
            if let Some(p) = &d.docs {
                if !docs_paths.contains(&p.as_str()) {
                    docs_paths.push(p.as_str());
                }
            }
        }
        let mut docs = Vec::with_capacity(docs_paths.len());
        for p in docs_paths {
            docs.push(read_artifact(dir, p)?);
        }

        let published = if dir.join(".cell").join("published.json").exists() {
            Some(read_artifact(dir, ".cell/published.json")?)
        } else {
            None
        };
        // The digest-gated sidecars ship only when they attest THIS
        // `cell.yaml` — a record `serve` would omit as stale has no business
        // in the artifact (it would still roll the workload's content hash).
        let digest = crate::context::sha256_hex(&cell_yaml.bytes);
        let sidecar = |rel: &str| -> Result<Option<ArtifactFile>> {
            let path = dir.join(rel);
            if !path.exists() {
                return Ok(None);
            }
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let stamped: Option<String> = serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .and_then(|v| {
                    v.get("cell_yaml_digest")
                        .and_then(|d| d.as_str())
                        .map(str::to_string)
                });
            if stamped.as_deref() != Some(digest.as_str()) {
                tracing::warn!(
                    file = rel,
                    "not shipping: its cell_yaml_digest does not match this cell.yaml (stale — \
                     re-run the command that writes it)"
                );
                return Ok(None);
            }
            read_artifact(dir, rel).map(Some)
        };
        let source_check = sidecar(".cell/source_check.json")?;
        let source_descriptions = sidecar(".cell/source_descriptions.json")?;
        let deployed_catalog = sidecar(".cell/deployed_catalog.json")?;
        let content_hash = content_hash(
            &cell_yaml,
            &sql,
            &docs,
            &published,
            &source_check,
            &source_descriptions,
            &deployed_catalog,
        );
        Ok(CellArtifact {
            dir: dir.to_path_buf(),
            cell_yaml,
            sql,
            docs,
            published,
            source_check,
            source_descriptions,
            deployed_catalog,
            content_hash,
        })
    }
}

fn read_artifact(dir: &Path, rel: &str) -> Result<ArtifactFile> {
    let path = dir.join(rel);
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading artifact file {}", path.display()))?;
    Ok(ArtifactFile {
        rel_path: rel.to_string(),
        bytes,
    })
}

fn content_hash(
    cell_yaml: &ArtifactFile,
    sql: &[ArtifactFile],
    docs: &[ArtifactFile],
    published: &Option<ArtifactFile>,
    source_check: &Option<ArtifactFile>,
    source_descriptions: &Option<ArtifactFile>,
    deployed_catalog: &Option<ArtifactFile>,
) -> String {
    let mut h = Sha256::new();
    feed(&mut h, cell_yaml);
    for f in sql {
        feed(&mut h, f);
    }
    for f in docs {
        feed(&mut h, f);
    }
    if let Some(f) = published {
        feed(&mut h, f);
    }
    if let Some(f) = source_check {
        feed(&mut h, f);
    }
    if let Some(f) = source_descriptions {
        feed(&mut h, f);
    }
    if let Some(f) = deployed_catalog {
        feed(&mut h, f);
    }
    let mut out = String::with_capacity(64);
    for b in h.finalize() {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Frame each file by its path before its bytes, so a rename or reorder changes
/// the hash (path and content are both part of the identity).
fn feed(h: &mut Sha256, f: &ArtifactFile) {
    h.update(f.rel_path.as_bytes());
    h.update([0u8]);
    h.update(&f.bytes);
    h.update([0u8]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_gathers_definition_and_transforms() {
        let dir = Path::new("test/integrations/orders");
        let def = CellDef::load(&dir.join("cell.yaml")).unwrap();
        let art = CellArtifact::collect(dir, "cell.yaml", &def).unwrap();

        assert_eq!(art.cell_yaml.rel_path, "cell.yaml");
        assert_eq!(art.sql.len(), 2); // stg_orders.sql + orders_daily.sql
        assert!(art.sql.iter().any(|f| f.rel_path == "sql/orders_daily.sql"));
        assert!(art.docs.is_empty(), "fixture cell declares no docs:");
        assert_eq!(art.content_hash.len(), 64); // hex SHA-256
    }

    #[test]
    fn content_hash_is_deterministic_and_path_sensitive() {
        let a = ArtifactFile {
            rel_path: "cell.yaml".into(),
            bytes: b"x".to_vec(),
        };
        let b = ArtifactFile {
            rel_path: "other.yaml".into(),
            bytes: b"x".to_vec(),
        };
        assert_eq!(
            content_hash(&a, &[], &[], &None, &None, &None, &None),
            content_hash(&a, &[], &[], &None, &None, &None, &None)
        );
        assert_ne!(
            content_hash(&a, &[], &[], &None, &None, &None, &None),
            content_hash(&b, &[], &[], &None, &None, &None, &None)
        );
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datamk-artifact-docs-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        dir
    }

    /// ADR 0013 §9: `collect` gathers every declared docs page — cell-level
    /// and export-level — and `content_hash` moves when a page's content
    /// changes, so a deploy that edits only prose still rolls the workload
    /// (a new ConfigMap name, `render.rs`'s `configmap_name`).
    #[test]
    fn collect_gathers_docs_pages_and_content_hash_moves_when_prose_changes() {
        let dir = tempdir("basic");
        std::fs::write(dir.join("docs/overview.md"), "Cell overview v1.").unwrap();
        std::fs::write(dir.join("docs/orders.md"), "Orders export v1.").unwrap();
        let yaml = "cell: c\n\
                    docs: docs/overview.md\n\
                    interface:\n\
                    \x20 - name: orders\n\
                    \x20   version: 1.0.0\n\
                    \x20   docs: docs/orders.md\n";
        std::fs::write(dir.join("cell.yaml"), yaml).unwrap();
        let def: CellDef = serde_yaml::from_str(yaml).unwrap();

        let art1 = CellArtifact::collect(&dir, "cell.yaml", &def).unwrap();
        assert_eq!(art1.docs.len(), 2);
        assert!(art1.docs.iter().any(|f| f.rel_path == "docs/overview.md"));
        assert!(art1.docs.iter().any(|f| f.rel_path == "docs/orders.md"));

        // Editing only a docs page's prose (description/version untouched)
        // must still move content_hash.
        std::fs::write(dir.join("docs/orders.md"), "Orders export v2 — edited.").unwrap();
        let art2 = CellArtifact::collect(&dir, "cell.yaml", &def).unwrap();
        assert_ne!(
            art1.content_hash, art2.content_hash,
            "a docs-only prose edit must change content_hash"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #16: `collect` picks up `.cell/source_check.json` exactly like
    /// `.cell/published.json` — present-when-it-exists, absent otherwise.
    /// The consequence that actually matters (a target must roll on a fresh
    /// live-verify) is covered end to end at the render layer
    /// (`kubernetes::render`'s `configmap_carries_source_check_and_content_hash_moves_when_it_changes`);
    /// this pins the collection half in isolation.
    #[test]
    fn collect_picks_up_source_check_when_present() {
        let dir = tempdir("source-check");
        let yaml = "cell: c\ninterface: []\n";
        std::fs::write(dir.join("cell.yaml"), yaml).unwrap();
        let def: CellDef = serde_yaml::from_str(yaml).unwrap();

        let without = CellArtifact::collect(&dir, "cell.yaml", &def).unwrap();
        assert!(without.source_check.is_none());

        std::fs::create_dir_all(dir.join(".cell")).unwrap();
        // A record stamped with another cell.yaml's digest is stale and
        // must not ship — `serve` would omit it anyway.
        std::fs::write(
            dir.join(".cell/source_check.json"),
            r#"{"outcome":"passed","checked_at":"t","datamk_version":"v","cell_yaml_digest":"d","profile":"p"}"#,
        )
        .unwrap();
        let stale = CellArtifact::collect(&dir, "cell.yaml", &def).unwrap();
        assert!(
            stale.source_check.is_none(),
            "a stale sidecar must not ship"
        );
        let digest = crate::context::cell_yaml_digest_of(&dir.join("cell.yaml")).unwrap();
        std::fs::write(
            dir.join(".cell/source_check.json"),
            format!(
                r#"{{"outcome":"passed","checked_at":"t","datamk_version":"v","cell_yaml_digest":"{digest}","profile":"p"}}"#
            ),
        )
        .unwrap();
        let with = CellArtifact::collect(&dir, "cell.yaml", &def).unwrap();
        assert!(with.source_check.is_some());
        assert_eq!(
            with.source_check.unwrap().rel_path,
            ".cell/source_check.json"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same file referenced by both the cell-level `docs:` and an
    /// export's collects exactly once — not a `configmap_key` collision
    /// waiting to happen in `render_configmap`.
    #[test]
    fn collect_deduplicates_a_docs_page_shared_by_cell_and_export() {
        let dir = tempdir("shared");
        std::fs::write(dir.join("docs/overview.md"), "Shared page.").unwrap();
        let yaml = "cell: c\n\
                    docs: docs/overview.md\n\
                    interface:\n\
                    \x20 - name: orders\n\
                    \x20   version: 1.0.0\n\
                    \x20   docs: docs/overview.md\n";
        std::fs::write(dir.join("cell.yaml"), yaml).unwrap();
        let def: CellDef = serde_yaml::from_str(yaml).unwrap();

        let art = CellArtifact::collect(&dir, "cell.yaml", &def).unwrap();
        assert_eq!(art.docs.len(), 1, "{:?}", art.docs);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR 0017 §4: the definitions file and a definition's own `docs:`
    /// page are collected into the same dedup-by-path pool as `docs:`, and
    /// a definitions-only edit rolls `content_hash` — the deploy artifact
    /// must not go stale behind a ConfigMap name that no longer matches
    /// what it serves.
    #[test]
    fn collect_gathers_definitions_and_content_hash_moves_on_a_definitions_only_edit() {
        let dir = tempdir("definitions");
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::write(dir.join("term.md"), "Net revenue, long form v1.").unwrap();
        std::fs::write(
            dir.join("definitions.yaml"),
            "definitions:\n  - term: net_revenue\n    description: Invoiced revenue.\n    \
             docs: term.md\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("cell.yaml"),
            "cell: c\ndefinitions: definitions.yaml\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("profiles/local.yaml"),
            "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
        )
        .unwrap();

        let def1 = CellDef::load(&dir.join("cell.yaml")).unwrap();
        let art1 = CellArtifact::collect(&dir, "cell.yaml", &def1).unwrap();
        assert!(
            art1.docs.iter().any(|f| f.rel_path == "definitions.yaml"),
            "{:?}",
            art1.docs
        );
        assert!(
            art1.docs.iter().any(|f| f.rel_path == "term.md"),
            "{:?}",
            art1.docs
        );

        // Editing only the definition's long-form page (description and
        // cell.yaml both untouched) must still move content_hash.
        std::fs::write(dir.join("term.md"), "Net revenue, long form v2 — edited.").unwrap();
        let def2 = CellDef::load(&dir.join("cell.yaml")).unwrap();
        let art2 = CellArtifact::collect(&dir, "cell.yaml", &def2).unwrap();
        assert_ne!(
            art1.content_hash, art2.content_hash,
            "a definitions-only edit must change content_hash"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
