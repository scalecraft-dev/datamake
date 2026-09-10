//! `semantic_model:` (ADR 0018 §1): the authoring surface naming where a
//! cell's Ossie documents live, plus the walk/merge that turns them into
//! one `Document`.

use anyhow::{anyhow, bail, Context as _, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use super::Document;

/// `semantic_model:`'s two shapes — exactly one of `dir` or `git`. Never a
/// scheme string (`git+https://…//osi@ref`, ADR 0018 §1): `@` collides with
/// URL userinfo and would leak a token into an error message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum SemanticModelSource {
    Dir {
        dir: String,
    },
    Git {
        git: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "ref")]
        r#ref: Option<String>,
    },
}

impl<'de> Deserialize<'de> for SemanticModelSource {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let value = serde_yaml::Value::deserialize(deserializer)?;
        let serde_yaml::Value::Mapping(map) = value else {
            return Err(D::Error::custom(
                "`semantic_model:` must be a map with exactly one of `dir` or `git` — never a \
                 scheme string (`git+https://…`): it would collide with URL userinfo and could \
                 leak a token into an error message.",
            ));
        };
        let has_dir = map.contains_key("dir");
        let has_git = map.contains_key("git");
        match (has_dir, has_git) {
            (true, true) => Err(D::Error::custom(
                "`semantic_model:` cannot declare both `dir` and `git` — pick one: a local \
                 directory of Ossie files, or a git repository.",
            )),
            (false, false) => Err(D::Error::custom(
                "`semantic_model:` must declare exactly one of `dir` or `git`.",
            )),
            (true, false) => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct DirHelper {
                    dir: String,
                }
                let h: DirHelper = serde_yaml::from_value(serde_yaml::Value::Mapping(map))
                    .map_err(|e| {
                        D::Error::custom(format!(
                            "`semantic_model.dir`: {e} — `path`/`ref` apply only to `git:`, not \
                             `dir:`."
                        ))
                    })?;
                Ok(SemanticModelSource::Dir { dir: h.dir })
            }
            (false, true) => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct GitHelper {
                    git: String,
                    #[serde(default)]
                    path: Option<String>,
                    #[serde(default, rename = "ref")]
                    r#ref: Option<String>,
                }
                let h: GitHelper = serde_yaml::from_value(serde_yaml::Value::Mapping(map))
                    .map_err(D::Error::custom)?;
                Ok(SemanticModelSource::Git {
                    git: h.git,
                    path: h.path,
                    r#ref: h.r#ref,
                })
            }
        }
    }
}

/// Resolve `semantic_model.dir` against the cell directory (ADR 0018 §1).
///
/// Unlike `config::docs::resolve_path`, `dir` is allowed to **leave** the
/// cell directory on purpose — the source of truth is the modeling repo,
/// not the cell — so this is a deliberately separate function, not a
/// widened `resolve_path`: relative only, canonicalized (closing `..` and
/// symlink escapes the same way), but with no containment requirement
/// beyond refusing to land inside `<cell>/profiles` or `<cell>/.cell` when
/// it happens to still resolve under the cell directory.
pub fn resolve_dir(cell_dir: &Path, raw: &str) -> Result<PathBuf> {
    let rel = Path::new(raw);
    if rel.is_absolute() {
        bail!(
            "`semantic_model.dir` '{raw}' must be a relative path — absolute paths are refused \
             (meaning must not vary by environment)."
        );
    }
    let cell_dir_canon = cell_dir.canonicalize().with_context(|| {
        format!(
            "resolving cell directory {} for `semantic_model.dir`",
            cell_dir.display()
        )
    })?;
    let candidate = cell_dir.join(rel);
    let resolved = candidate
        .canonicalize()
        .with_context(|| format!("resolving `semantic_model.dir: {raw}`"))?;
    if let Ok(inside) = resolved.strip_prefix(&cell_dir_canon) {
        if inside.starts_with("profiles") {
            bail!(
                "`semantic_model.dir: {raw}` resolves into the profile directory — it must not \
                 expose environment config."
            );
        }
        if inside.starts_with(".cell") {
            bail!(
                "`semantic_model.dir: {raw}` resolves into datamk's private state directory \
                 (.cell) — it must not reference engine-internal files."
            );
        }
    }
    Ok(resolved)
}

/// Files-in-one-`semantic_model:`-source caps (ADR 0018 §2).
pub const MAX_FILES: usize = 2048;
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 32 * 1024 * 1024;

/// Walk `root` (a directory, recursively, or a single file) collecting
/// every `.yaml`/`.yml`/`.json` file's relative path and bytes, sorted by
/// relative path (ADR 0018 §2). Dot-directories and dot-files are skipped
/// entirely (never descended into, never read). Every entry is
/// canonicalized and checked to still resolve under the canonicalized
/// `root` — closing a symlink escape the same way `config::docs::
/// resolve_path` does.
pub fn walk(root: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let root_canon = root
        .canonicalize()
        .with_context(|| format!("resolving semantic model source {}", root.display()))?;

    if root_canon.is_file() {
        let rel = root_canon
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let bytes = read_capped(&root_canon, &rel)?;
        return Ok(vec![(rel, bytes)]);
    }

    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root_canon.clone()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .with_context(|| format!("reading directory {}", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading directory {}", dir.display()))?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let canon = path
                .canonicalize()
                .with_context(|| format!("resolving {}", path.display()))?;
            if !canon.starts_with(&root_canon) {
                bail!(
                    "{} escapes the semantic model walk root via a symlink — refused.",
                    path.display()
                );
            }
            let is_dir = std::fs::metadata(&path)
                .map(|m| m.is_dir())
                .unwrap_or(false);
            if is_dir {
                // M4: `semantic_model.dir` is allowed to leave the cell
                // directory (ADR 0018 §1) — a `dir: ..` (or any ancestor)
                // walk would otherwise cross back into `profiles/` (a
                // sibling or the cell's own environment config) or `.cell/`
                // (datamk's private state) at whatever depth they turn up,
                // not just when `dir:` resolves directly into one of them
                // (`resolve_dir`'s own check). `.cell` is also a
                // dot-directory (skipped above already); named here too so
                // the rule doesn't quietly depend on that coincidence.
                if name_str == "profiles" || name_str == ".cell" {
                    continue;
                }
                stack.push(path);
                continue;
            }
            // M4: never treat a `cell.yaml` as an Ossie file — it has a
            // `.yaml` extension and would otherwise match below, echoing
            // the contract's own top-level keys into a "not an Ossie
            // document" error when a `dir:` walk crosses one.
            if name_str == "cell.yaml" {
                continue;
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase);
            if matches!(ext.as_deref(), Some("yaml") | Some("yml") | Some("json")) {
                files.push(path);
                if files.len() > MAX_FILES {
                    bail!(
                        "semantic model source under {} has more than {MAX_FILES} files — trim \
                         it (split into multiple cells, or narrow `dir:`).",
                        root.display()
                    );
                }
            }
        }
    }

    let mut total: u64 = 0;
    let mut result = Vec::with_capacity(files.len());
    for path in files {
        let rel = path
            .strip_prefix(&root_canon)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = read_capped(&path, &rel)?;
        total += bytes.len() as u64;
        if total > MAX_TOTAL_BYTES {
            bail!(
                "semantic model source under {} exceeds {MAX_TOTAL_BYTES} bytes merged — trim \
                 it.",
                root.display()
            );
        }
        result.push((rel, bytes));
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}

fn read_capped(path: &Path, rel: &str) -> Result<Vec<u8>> {
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.len() > MAX_FILE_BYTES {
        bail!(
            "{rel} is {} bytes (max {MAX_FILE_BYTES}) — refused.",
            meta.len()
        );
    }
    std::fs::read(path).with_context(|| format!("reading {}", path.display()))
}

/// The merge of every file `walk` found (ADR 0018 §2): concatenated
/// `semantic_model[]` in file order, plus the merged-content digest and the
/// file each semantic model came from.
#[derive(Debug)]
pub struct Merged {
    pub document: Document,
    pub files: Vec<String>,
    /// semantic model name -> the relative file it was defined in.
    pub model_files: IndexMap<String, String>,
    pub content_sha256: String,
}

/// Merge `walk`'s output (already sorted by relative path — the order this
/// function both concatenates `semantic_model[]` in and hashes over) into
/// one `Document`. Every file must parse and agree on `version`; semantic
/// model names must be unique across the whole source; dataset names must
/// be unique within their model (the spec's own rule — nothing cross-model).
pub fn merge(files: &[(String, Vec<u8>)]) -> Result<Merged> {
    if files.is_empty() {
        bail!(
            "no Ossie files (.yaml/.yml/.json) found — a `semantic_model:` source must contain \
             at least one."
        );
    }

    let mut version: Option<(String, String)> = None; // (version, first file that declared it)
    let mut semantic_model = Vec::new();
    let mut dialects = Vec::new();
    let mut vendors = Vec::new();
    let mut model_files: IndexMap<String, String> = IndexMap::new();

    for (rel, bytes) in files {
        let text =
            std::str::from_utf8(bytes).map_err(|e| anyhow!("{rel} is not valid UTF-8: {e}"))?;
        let doc = Document::parse_str(text, rel)?;

        match &version {
            None => version = Some((doc.version.clone(), rel.clone())),
            Some((v, first_file)) if v != &doc.version => {
                bail!(
                    "mixed Ossie versions in one `semantic_model:` source: {first_file} \
                     declares version {v}, {rel} declares version {} — every file in one source \
                     must agree.",
                    doc.version
                );
            }
            _ => {}
        }

        for m in doc.semantic_model {
            if let Some(prev) = model_files.get(&m.name) {
                bail!(
                    "semantic model '{}' is defined in both {prev} and {rel} — semantic model \
                     names must be unique across a `semantic_model:` source.",
                    m.name
                );
            }
            let mut seen_datasets = std::collections::HashSet::new();
            for ds in &m.datasets {
                if !seen_datasets.insert(ds.name.clone()) {
                    bail!(
                        "{rel}: semantic model '{}' declares dataset '{}' twice — dataset names \
                         must be unique within a model.",
                        m.name,
                        ds.name
                    );
                }
            }
            model_files.insert(m.name.clone(), rel.clone());
            semantic_model.push(m);
        }
        dialects.extend(doc.dialects);
        vendors.extend(doc.vendors);
    }

    let document = Document {
        version: version.map(|(v, _)| v).unwrap_or_default(),
        dialects: dedup_preserve(dialects),
        vendors: dedup_preserve(vendors),
        semantic_model,
    };

    let mut hasher = Sha256::new();
    for (rel, bytes) in files {
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        hasher.update(bytes);
        hasher.update([0u8]);
    }
    let content_sha256 = hex(&hasher.finalize());

    Ok(Merged {
        document,
        files: files.iter().map(|(rel, _)| rel.clone()).collect(),
        model_files,
        content_sha256,
    })
}

fn dedup_preserve<T: PartialEq>(items: Vec<T>) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    for it in items {
        if !out.contains(&it) {
            out.push(it);
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datamk-ossie-source-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn parse(yaml: &str) -> SemanticModelSource {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn source_dir_shape() {
        assert_eq!(
            parse("dir: ../osi\n"),
            SemanticModelSource::Dir {
                dir: "../osi".to_string()
            }
        );
    }

    #[test]
    fn source_git_with_path_and_ref() {
        assert_eq!(
            parse("git: https://example.com/x.git\npath: osi\nref: v1\n"),
            SemanticModelSource::Git {
                git: "https://example.com/x.git".to_string(),
                path: Some("osi".to_string()),
                r#ref: Some("v1".to_string()),
            }
        );
    }

    #[test]
    fn source_both_dir_and_git_is_an_error() {
        let err: std::result::Result<SemanticModelSource, _> =
            serde_yaml::from_str("dir: a\ngit: b\n");
        assert!(err.is_err());
    }

    #[test]
    fn source_path_without_git_is_an_error() {
        let err: std::result::Result<SemanticModelSource, _> =
            serde_yaml::from_str("dir: a\npath: sub\n");
        assert!(err.is_err());
    }

    #[test]
    fn source_string_is_an_error() {
        let err: std::result::Result<SemanticModelSource, _> = serde_yaml::from_str("a-string");
        assert!(err.is_err());
    }

    #[test]
    fn walk_orders_by_relative_path_and_skips_dot_entries() {
        let dir = tempdir("order");
        std::fs::write(dir.join("b.yaml"), "b").unwrap();
        std::fs::write(dir.join("a.yml"), "a").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/c.json"), "c").unwrap();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        std::fs::write(dir.join(".hidden/d.yaml"), "d").unwrap();
        std::fs::write(dir.join(".dotfile.yaml"), "e").unwrap();
        std::fs::write(dir.join("ignored.txt"), "f").unwrap();

        let found = walk(&dir).unwrap();
        let names: Vec<&str> = found.iter().map(|(rel, _)| rel.as_str()).collect();
        assert_eq!(names, vec!["a.yml", "b.yaml", "sub/c.json"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_skips_profiles_and_dotcell_at_any_depth_and_never_reads_cell_yaml() {
        // The shape a `dir: ..` (or any wide `dir:`) produces: the cell's
        // own `cell.yaml`, `profiles/`, and `.cell/` sitting next to the
        // Ossie source, plus a nested `sub/profiles` — none of it should be
        // walked.
        let dir = tempdir("wide-dir");
        std::fs::write(dir.join("cell.yaml"), "cell: t\ndiscover:\n  never: true\n").unwrap();
        std::fs::write(dir.join("osi.yaml"), "osi").unwrap();
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::write(dir.join("profiles/prod.yaml"), "warehouse: prod").unwrap();
        std::fs::create_dir_all(dir.join(".cell")).unwrap();
        std::fs::write(dir.join(".cell/semantic_model.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("sub/profiles")).unwrap();
        std::fs::write(dir.join("sub/profiles/dev.yaml"), "warehouse: dev").unwrap();
        std::fs::create_dir_all(dir.join("sub/.cell")).unwrap();
        std::fs::write(dir.join("sub/.cell/leak.yaml"), "leak").unwrap();

        let found = walk(&dir).unwrap();
        let names: Vec<&str> = found.iter().map(|(rel, _)| rel.as_str()).collect();
        assert_eq!(names, vec!["osi.yaml"], "{names:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_accepts_a_single_file() {
        let dir = tempdir("single");
        std::fs::write(dir.join("one.yaml"), "one").unwrap();
        let found = walk(&dir.join("one.yaml")).unwrap();
        assert_eq!(found, vec![("one.yaml".to_string(), b"one".to_vec())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_refuses_a_symlink_that_escapes_the_root() {
        let dir = tempdir("symlink-escape");
        let outside = tempdir("symlink-escape-outside");
        std::fs::write(outside.join("secret.yaml"), "s").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("secret.yaml"), dir.join("link.yaml")).unwrap();
            let err = walk(&dir).unwrap_err();
            assert!(err.to_string().contains("escapes"));
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn walk_caps_file_count() {
        let dir = tempdir("caps");
        for i in 0..(MAX_FILES + 1) {
            std::fs::write(dir.join(format!("f{i}.yaml")), "x").unwrap();
        }
        let err = walk(&dir).unwrap_err();
        assert!(err.to_string().contains("more than"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn doc_file(rel: &str, name: &str, version: &str) -> (String, Vec<u8>) {
        let text = format!(
            "version: {version}\nsemantic_model:\n  - name: {name}\n    datasets:\n      - \
             name: d\n        source: s\n"
        );
        (rel.to_string(), text.into_bytes())
    }

    #[test]
    fn merge_collides_on_duplicate_model_name_across_files() {
        let files = vec![
            doc_file("a.yaml", "m", "0.1.1"),
            doc_file("b.yaml", "m", "0.1.1"),
        ];
        let err = merge(&files).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("a.yaml") && msg.contains("b.yaml"));
    }

    #[test]
    fn merge_refuses_mixed_versions() {
        let files = vec![
            doc_file("a.yaml", "m1", "0.1.1"),
            doc_file("b.yaml", "m2", "0.2.0.dev0"),
        ];
        let err = merge(&files).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mixed Ossie versions"));
        assert!(msg.contains("a.yaml") && msg.contains("b.yaml"));
    }

    #[test]
    fn walk_and_merge_the_checked_in_ossie_fixtures() {
        let root = Path::new("test/fixtures/ossie");
        let files = walk(root).unwrap();
        let names: Vec<&str> = files.iter().map(|(rel, _)| rel.as_str()).collect();
        assert_eq!(
            names,
            vec!["invoice.yaml", "public.yaml", "sub/marketing.yaml"]
        );

        let merged = merge(&files).unwrap();
        assert_eq!(merged.document.semantic_model.len(), 3);
        let names: std::collections::HashSet<&str> = merged
            .document
            .semantic_model
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(
            names,
            std::collections::HashSet::from(["invoice", "public", "marketing"])
        );
        assert_eq!(merged.model_files["invoice"], "invoice.yaml");
        assert_eq!(merged.model_files["marketing"], "sub/marketing.yaml");
    }

    #[test]
    fn merge_is_deterministic() {
        let files = vec![doc_file("a.yaml", "m1", "0.1.1")];
        let m1 = merge(&files).unwrap();
        let m2 = merge(&files).unwrap();
        assert_eq!(m1.content_sha256, m2.content_sha256);
        assert_eq!(m1.document.semantic_model.len(), 1);
        assert_eq!(m1.model_files["m1"], "a.yaml");
    }
}
