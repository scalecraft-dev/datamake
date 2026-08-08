//! Long-form docs pages (ADR 0013): a `docs:` field on the cell and each
//! export naming one relative file, additive to `description` — never a
//! replacement. Delivered inline in the context document only under `GET
//! /context?include=docs`; there is **no** `/docs/:name` route (ADR 0012 §4:
//! one document, one route).
//!
//! Path resolution is security-critical: the profile Secret mounts at
//! `/cell/profiles`, *inside* the cell directory
//! (`deploy/targets/kubernetes/render.rs`'s `PROFILE_MOUNT`), so "resolves
//! under the cell directory" alone is not a sufficient check —
//! `docs: profiles/prod.yaml` needs no `..` to expose `s3.key_id`/`s3.secret`.
//! Every path is allowlisted by construction: relative only, canonicalized
//! (resolving symlinks), required to land under the canonicalized cell
//! directory, and explicitly denied entry into `profiles/` and `.cell/`.

use anyhow::{bail, Context as _, Result};
use std::path::{Path, PathBuf};

use super::{CellDef, Export};

/// Per-page cap (ADR 0013): the k8s ConfigMap that delivers cell content is
/// capped at 1 MiB by the API server, shared with `cell.yaml`, every
/// transform's SQL, and `published.json`.
pub const MAX_PAGE_BYTES: usize = 64 * 1024;
/// Total cap across every docs page declared on a cell — the context
/// document is one request an agent loads whole.
pub const MAX_TOTAL_BYTES: usize = 256 * 1024;

/// One loaded docs page: identity plus content, read exactly once.
#[derive(Debug, Clone)]
pub struct DocsPage {
    /// `"cell"` or the route key (`name@major`).
    pub target: String,
    /// The declared relative path, verbatim. Not read by any caller today —
    /// `declared.docs`' identity is built independently, directly from
    /// `def.docs`/`export.docs` (`context::docs_entries`), so a page never
    /// needs to be loaded just to report its own declared path — kept on
    /// the struct for diagnostics and API cohesion (identity + content
    /// together), mirroring `CellArtifact::dir`'s same rationale.
    #[allow(dead_code)]
    pub path: String,
    pub media_type: String,
    pub content: std::sync::Arc<str>,
    pub sha256: String,
    pub bytes: usize,
}

/// `text/markdown; charset=utf-8` for `.md`/`.markdown`, `text/plain;
/// charset=utf-8` otherwise. No sniffing beyond the extension — the shape is
/// closed (one relative path, no globs), so the extension is all there is.
pub(crate) fn guess_media_type(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".md") || lower.ends_with(".markdown") {
        "text/markdown; charset=utf-8"
    } else {
        "text/plain; charset=utf-8"
    }
}

/// The subject clause a message opens with: `cell` (no export name) or
/// `export '{name}':`. Every docs error is phrased around one of these two
/// shapes, matching `validate_prose`'s existing "cell `description`" /
/// "export '{name}': `description`" split.
fn clause(export_name: Option<&str>) -> String {
    match export_name {
        Some(n) => format!("export '{n}':"),
        None => "cell".to_string(),
    }
}

/// `export '{name}'` / `the cell` — the noun form used inside the `reading
/// docs page ... (referenced by ...)` anyhow context, distinct from
/// `clause`'s statement-opening form.
fn clause_subject(export_name: Option<&str>) -> String {
    match export_name {
        Some(n) => format!("export '{n}'"),
        None => "the cell".to_string(),
    }
}

/// Resolve a declared `docs:` path against the cell directory, refusing
/// anything outside an allowlist-by-construction:
///
/// 1. Relative only — an absolute path is rejected outright.
/// 2. Canonicalized (resolving symlinks) and required to land under the
///    canonicalized cell directory — closes both `..` escapes and symlink
///    escapes with the same check.
/// 3. Explicitly denied entry into `profiles/` (the profile Secret mounts
///    there, `deploy/targets/kubernetes/render.rs`'s `PROFILE_MOUNT`) and
///    `.cell/` (engine-owned state, e.g. `published.json`).
fn resolve_path(cell_dir: &Path, raw: &str, export_name: Option<&str>) -> Result<PathBuf> {
    let rel = Path::new(raw);
    if rel.is_absolute() {
        bail!(
            "`docs:` path '{raw}' must be a relative path inside the cell directory — \
             absolute paths and `..` are rejected."
        );
    }

    let cell_dir_canon = cell_dir.canonicalize().with_context(|| {
        format!(
            "resolving cell directory {} for `docs:`",
            cell_dir.display()
        )
    })?;
    let candidate = cell_dir.join(rel);
    let resolved = candidate.canonicalize().with_context(|| {
        format!(
            "reading docs page {raw} (referenced by `docs:` on {})",
            clause_subject(export_name)
        )
    })?;
    if !resolved.starts_with(&cell_dir_canon) {
        bail!(
            "`docs:` path '{raw}' must be a relative path inside the cell directory — \
             absolute paths and `..` are rejected."
        );
    }

    let inside = resolved.strip_prefix(&cell_dir_canon).unwrap_or(&resolved);
    if inside.starts_with("profiles") {
        bail!(
            "{} `docs` path {raw} resolves into the profile directory — docs must not expose \
             environment config.",
            clause(export_name)
        );
    }
    if inside.starts_with(".cell") {
        bail!(
            "{} `docs` path {raw} resolves into datamk's private state directory (.cell) — \
             docs must not reference engine-internal files.",
            clause(export_name)
        );
    }
    Ok(resolved)
}

/// Resolve, read, cap-check, and UTF-8-validate one declared page. Returns
/// the resolved absolute path and its content. Every failure mode fails
/// loud: unreadable, oversized, empty, or non-UTF-8 — matching
/// `load_principals`'s discipline for the profile's `principals:` file.
fn read_one(cell_dir: &Path, raw: &str, export_name: Option<&str>) -> Result<(PathBuf, String)> {
    let resolved = resolve_path(cell_dir, raw, export_name)?;
    let bytes = std::fs::read(&resolved).with_context(|| {
        format!(
            "reading docs page {raw} (referenced by `docs:` on {})",
            clause_subject(export_name)
        )
    })?;
    if bytes.len() > MAX_PAGE_BYTES {
        bail!(
            "{} docs page {raw} is {} bytes (max {MAX_PAGE_BYTES}) — the context document is \
             one request an agent loads whole; split the page or link out of it.",
            clause(export_name),
            bytes.len()
        );
    }
    if bytes.is_empty() {
        bail!(
            "{} docs page {raw} is empty — an empty `docs:` page has nothing for an agent to \
             read; remove `docs:` or add content.",
            clause(export_name)
        );
    }
    let content = String::from_utf8(bytes).map_err(|_| {
        anyhow::anyhow!(
            "docs page {raw} is not valid UTF-8 — the context document is JSON; docs must be \
             UTF-8 text."
        )
    })?;
    Ok((resolved, content))
}

/// Validate every declared `docs:` page — cell-level plus every export
/// (private and public alike; a malformed page is a `cell.yaml` error
/// regardless of whether it would ever be served) — resolving paths,
/// checking the per-page cap, and checking the total cap across all pages.
/// Called from `CellDef::load`, next to `validate_prose`, on every parse:
/// every caller of `config::load` (the engine, `verify`, `release`,
/// `deploy`, `serve`, `datamk context`) gets a validated cell before
/// anything opens a connection.
pub fn validate_all(cell_dir: &Path, def: &CellDef) -> Result<()> {
    let mut total = 0usize;
    if let Some(raw) = &def.docs {
        let (_, content) = read_one(cell_dir, raw, None)?;
        total += content.len();
    }
    for export in &def.interface {
        if let Some(raw) = &export.docs {
            let (_, content) = read_one(cell_dir, raw, Some(&export.name))?;
            total += content.len();
        }
    }
    if total > MAX_TOTAL_BYTES {
        bail!(
            "docs pages total {total} bytes (max {MAX_TOTAL_BYTES}) — the context document is \
             one request; trim the pages, or drop `docs:` from the exports that don't need one."
        );
    }
    Ok(())
}

/// Load the full `DocsPage` list — content, sha256, media type — for the
/// cell (if declared) plus every **discoverable** export (`routes`, the
/// same visibility-filtered list `context::declared` reads): a private
/// export's docs page never reaches this list, matching ADR 0012 §4 — "a
/// private export appears nowhere, in any form, not even as a name."
///
/// Called at `serve` startup (cached in `AppState`, never touched again for
/// the life of the process — the mount is immutable for a pod's lifetime)
/// and at `datamk context` emit time (unless `--no-docs`). Never on a
/// request path.
pub fn load_declared(
    dir: &Path,
    def: &CellDef,
    routes: &[(String, Export)],
) -> Result<Vec<DocsPage>> {
    let mut pages = Vec::new();
    if let Some(raw) = &def.docs {
        pages.push(build_page(dir, "cell", raw, None)?);
    }
    for (route, export) in routes {
        if let Some(raw) = &export.docs {
            pages.push(build_page(dir, route, raw, Some(&export.name))?);
        }
    }
    Ok(pages)
}

fn build_page(dir: &Path, target: &str, raw: &str, export_name: Option<&str>) -> Result<DocsPage> {
    let (_, content) = read_one(dir, raw, export_name)?;
    let sha256 = crate::context::sha256_hex(content.as_bytes());
    let bytes = content.len();
    Ok(DocsPage {
        target: target.to_string(),
        path: raw.to_string(),
        media_type: guess_media_type(raw).to_string(),
        content: std::sync::Arc::from(content),
        sha256,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datamk-docs-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn def_with(cell_docs: Option<&str>, export_docs: Option<&str>) -> CellDef {
        let mut yaml = "cell: c\n".to_string();
        if let Some(d) = cell_docs {
            yaml += &format!("docs: {d}\n");
        }
        yaml += "interface:\n  - name: e\n    version: 1.0.0\n";
        if let Some(d) = export_docs {
            yaml += &format!("    docs: {d}\n");
        }
        serde_yaml::from_str(&yaml).unwrap()
    }

    // --- security: path resolution -----------------------------------

    #[test]
    fn rejects_a_docs_path_into_the_profile_directory() {
        let dir = tempdir("profiles-reject");
        fs::create_dir_all(dir.join("profiles")).unwrap();
        fs::write(dir.join("profiles/prod.yaml"), "s3:\n  secret: x\n").unwrap();
        let def = def_with(None, Some("profiles/prod.yaml"));
        let err = validate_all(&dir, &def).unwrap_err().to_string();
        assert!(err.contains("resolves into the profile directory"), "{err}");
        assert!(
            err.contains("docs must not expose environment config"),
            "{err}"
        );
    }

    #[test]
    fn rejects_a_docs_path_using_dotdot_to_escape_the_cell_dir() {
        let dir = tempdir("dotdot-reject");
        fs::write(dir.parent().unwrap().join("outside.md"), "secret").unwrap();
        let def = def_with(Some("../outside.md"), None);
        let err = validate_all(&dir, &def).unwrap_err().to_string();
        assert!(
            err.contains("must be a relative path inside the cell directory"),
            "{err}"
        );
    }

    #[test]
    fn rejects_an_absolute_docs_path() {
        let dir = tempdir("abs-reject");
        let def = def_with(Some("/etc/datamk/principals.json"), None);
        let err = validate_all(&dir, &def).unwrap_err().to_string();
        assert!(
            err.contains("must be a relative path inside the cell directory"),
            "{err}"
        );
    }

    #[test]
    fn rejects_a_docs_path_reaching_the_private_principals_file_via_dotdot() {
        let dir = tempdir("principals-reject");
        let outside = dir.parent().unwrap().join("principals.json");
        fs::write(&outside, r#"{"tok": ["role"]}"#).unwrap();
        let def = def_with(None, Some("../principals.json"));
        let err = validate_all(&dir, &def).unwrap_err().to_string();
        assert!(
            err.contains("must be a relative path inside the cell directory"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlink_that_escapes_the_cell_dir() {
        let dir = tempdir("symlink-reject");
        let outside = dir
            .parent()
            .unwrap()
            .join(format!("datamk-docs-symlink-target-{}", std::process::id()));
        fs::write(&outside, "secret content").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("escape.md")).unwrap();
        let def = def_with(Some("escape.md"), None);
        let err = validate_all(&dir, &def).unwrap_err().to_string();
        assert!(
            err.contains("must be a relative path inside the cell directory"),
            "{err}"
        );
    }

    #[test]
    fn rejects_a_docs_path_into_dot_cell() {
        let dir = tempdir("dotcell-reject");
        fs::create_dir_all(dir.join(".cell")).unwrap();
        fs::write(dir.join(".cell/published.json"), "{}").unwrap();
        let def = def_with(Some(".cell/published.json"), None);
        let err = validate_all(&dir, &def).unwrap_err().to_string();
        assert!(err.contains("private state directory (.cell)"), "{err}");
    }

    // --- caps -----------------------------------------------------------

    #[test]
    fn rejects_a_page_over_the_per_page_cap() {
        let dir = tempdir("page-cap");
        fs::write(dir.join("big.md"), "x".repeat(MAX_PAGE_BYTES + 1)).unwrap();
        let def = def_with(None, Some("big.md"));
        let err = validate_all(&dir, &def).unwrap_err().to_string();
        assert!(err.contains("is 65537 bytes (max 65536)"), "{err}");
        assert!(err.contains("export 'e':"), "{err}");
    }

    #[test]
    fn rejects_pages_over_the_total_cap() {
        // Every individual page must stay under the per-page cap (65536), so
        // exceeding the total (262144) needs at least 5 max-sized pages:
        // cell + 4 exports.
        let dir = tempdir("total-cap");
        fs::write(dir.join("cell.md"), "x".repeat(MAX_PAGE_BYTES)).unwrap();
        let mut def = def_with(Some("cell.md"), None);
        for i in 0..4 {
            let name = format!("export{i}.md");
            fs::write(dir.join(&name), "x".repeat(MAX_PAGE_BYTES)).unwrap();
            def.interface.push(crate::config::Export {
                name: format!("e{i}"),
                version: "1.0.0".to_string(),
                source: None,
                description: None,
                docs: Some(name),
                grain: vec![],
                schema: Default::default(),
                freshness: None,
                visibility: crate::config::Visibility::default(),
                contract: crate::config::Contract::default(),
            });
        }
        let err = validate_all(&dir, &def).unwrap_err().to_string();
        assert!(err.contains("docs pages total"), "{err}");
        assert!(err.contains("max 262144"), "{err}");
    }

    #[test]
    fn rejects_an_empty_page() {
        let dir = tempdir("empty-page");
        fs::write(dir.join("empty.md"), "").unwrap();
        let def = def_with(Some("empty.md"), None);
        let err = validate_all(&dir, &def).unwrap_err().to_string();
        assert!(err.contains("is empty"), "{err}");
    }

    #[test]
    fn rejects_non_utf8_content() {
        let dir = tempdir("non-utf8");
        fs::write(dir.join("bad.md"), [0xff, 0xfe, 0x00, 0xff]).unwrap();
        let def = def_with(Some("bad.md"), None);
        let err = validate_all(&dir, &def).unwrap_err().to_string();
        assert!(err.contains("not valid UTF-8"), "{err}");
    }

    #[test]
    fn a_well_formed_page_validates_and_loads() {
        let dir = tempdir("ok");
        fs::write(dir.join("overview.md"), "# Orders\n\nWhat one row means.").unwrap();
        let def = def_with(Some("overview.md"), None);
        validate_all(&dir, &def).expect("well-formed docs must pass");

        let routes = crate::context::discoverable_routes(&def).unwrap();
        let pages = load_declared(&dir, &def, &routes).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].target, "cell");
        assert_eq!(pages[0].path, "overview.md");
        assert_eq!(pages[0].media_type, "text/markdown; charset=utf-8");
        assert!(pages[0].content.starts_with("# Orders"));
        assert_eq!(pages[0].sha256.len(), 64);
    }

    #[test]
    fn media_type_falls_back_to_plain_text_for_non_markdown() {
        assert_eq!(guess_media_type("notes.md"), "text/markdown; charset=utf-8");
        assert_eq!(
            guess_media_type("NOTES.MARKDOWN"),
            "text/markdown; charset=utf-8"
        );
        assert_eq!(guess_media_type("notes.txt"), "text/plain; charset=utf-8");
    }

    #[test]
    fn a_cell_with_no_docs_fields_validates_trivially() {
        let def = def_with(None, None);
        // No filesystem access needed at all — the cell dir need not exist.
        validate_all(Path::new("/nonexistent/does/not/matter"), &def)
            .expect("absent docs: fields must not touch the filesystem");
    }
}
