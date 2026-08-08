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
    /// SHA-256 over (cell_yaml ++ sql ++ docs ++ published), each entry framed
    /// by its `rel_path`. A stable content identity: re-releasing (a new pin)
    /// or editing a docs page changes it, which a target uses to roll the
    /// workload (ADR 0002, ADR 0013 §9).
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

        // ADR 0013 §9: every declared `docs:` path, cell-level and every
        // export's, deduplicated by relative path before reading — the same
        // page may be named by more than one `docs:` field.
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
        let mut docs = Vec::with_capacity(docs_paths.len());
        for p in docs_paths {
            docs.push(read_artifact(dir, p)?);
        }

        let published = if dir.join(".cell").join("published.json").exists() {
            Some(read_artifact(dir, ".cell/published.json")?)
        } else {
            None
        };

        let content_hash = content_hash(&cell_yaml, &sql, &docs, &published);
        Ok(CellArtifact {
            dir: dir.to_path_buf(),
            cell_yaml,
            sql,
            docs,
            published,
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
            content_hash(&a, &[], &[], &None),
            content_hash(&a, &[], &[], &None)
        );
        assert_ne!(
            content_hash(&a, &[], &[], &None),
            content_hash(&b, &[], &[], &None)
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
}
