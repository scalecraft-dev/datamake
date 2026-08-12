//! The project file (`datamk.yaml`, ADR 0014): the cells one `datamk serve`
//! process mounts behind one port. Composition only — it names *which* cells
//! and *where* they mount, never what a cell means. A cell's identity, its
//! interface, and its bindings stay in its own directory; nothing here can
//! change what a cell serves, only whether this process serves it.
//!
//! It is deliberately **not** a mesh manifest (`mesh.rs`) and deliberately
//! not a registry: every entry is a local path the operator already owns,
//! resolved off the filesystem at startup. Nothing is fetched, nothing is
//! indexed, and no cell learns that another exists.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The project-file schema version. The discriminator `-f` dispatches on
/// (`datamk:` = project, `cell:` = cell), which is why it is required rather
/// than defaulted — same discipline as `datamk_mesh`/`datamk_context`.
pub const DATAMK_PROJECT_VERSION: u32 = 1;

/// The file name discovered when `--file` is omitted, preferred over
/// `cell.yaml`.
pub const DEFAULT_FILE: &str = "datamk.yaml";

/// The URL path segment a cell mounts at. Deliberately narrower than a cell
/// name (which is unconstrained today) and narrower than DuckDB's identifier
/// grammar (which rejects `flight-spend`): `@` is excluded because export
/// routes carry `@major` and `/a@1/b@1` is unreadable; `.` and `/` because
/// they are path structure; everything else because a mount ends up in a URL
/// an operator types by hand.
fn valid_mount(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The parsed file, before path resolution or cell opening.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFile {
    datamk: u32,
    /// The profile every cell uses unless it names its own. Absent = `local`,
    /// matching every other command's default.
    #[serde(default)]
    profile: Option<String>,
    cells: Vec<Entry>,
}

/// A `cells:` entry: a bare path, or the same path plus overrides. The
/// shorthand is honest about what it omits — it is a *path*, and the mount
/// comes from the cell's own declared name, never from the directory it
/// happens to be checked out into.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Entry {
    Path(String),
    Full(FullEntry),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FullEntry {
    path: String,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    mount: Option<String>,
    #[serde(default)]
    no_data: bool,
}

/// One resolved entry: an absolute `cell.yaml` path, the profile to open it
/// with, and the mount — except the mount, which cannot be known until the
/// cell's own `cell:` name is read (see `Project::mount_for`).
#[derive(Debug, Clone)]
pub struct ProjectCell {
    /// Absolute path to the cell definition. **Canonicalized at load**: the
    /// poller re-opens the cell from this path on every swap
    /// (`serve::spawn_poller`), and a relative path resolved against the
    /// process cwd would work at startup and then fail 15 seconds later with
    /// nothing but a `tracing::warn!` — the cell would silently freeze on its
    /// opening execution.
    pub file: PathBuf,
    pub profile: String,
    /// The explicit `mount:` override, if the author wrote one. `None` means
    /// "use the cell's declared name", which cannot be read without parsing
    /// the cell.
    pub mount: Option<String>,
    pub no_data: bool,
    /// 0-based index in `cells:`, for error messages that point at the entry
    /// the author has to go edit.
    pub index: usize,
}

#[derive(Debug, Clone)]
pub struct Project {
    /// The project file itself, for error messages.
    pub file: PathBuf,
    pub cells: Vec<ProjectCell>,
}

/// Does this file look like a project file rather than a cell definition?
/// Dispatches on the top-level key alone — a cheap, total check that never
/// fully parses, so an invalid *project* file still reaches `load` and fails
/// with a project-shaped error instead of "this isn't a cell".
pub fn is_project_file(path: &Path) -> Result<bool> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let doc: serde_yaml::Value = serde_yaml::from_str(&raw)
        .with_context(|| format!("parsing {} as YAML", path.display()))?;
    let has = |k: &str| doc.get(k).is_some();
    match (has("datamk"), has("cell")) {
        (true, _) => Ok(true),
        (false, true) => Ok(false),
        (false, false) => bail!(
            "{} is neither a cell definition (top-level `cell:`) nor a project file \
             (top-level `datamk:`)",
            path.display()
        ),
    }
}

/// Resolve `--file` when it was omitted: `datamk.yaml` in the current
/// directory, else `cell.yaml`, else an error naming both.
pub fn discover() -> Result<PathBuf> {
    for name in [DEFAULT_FILE, "cell.yaml"] {
        let p = PathBuf::from(name);
        if p.is_file() {
            return Ok(p);
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    bail!(
        "no cell.yaml or {DEFAULT_FILE} in {} — pass --file <path>/cell.yaml, or run \
         `datamk init <name>` to scaffold a cell",
        cwd.display()
    )
}

/// Parse and validate a project file. Every *authoring* error (bad version,
/// empty list, missing cell definition, illegal or duplicated mount) surfaces
/// here, in one pass, before any cell is opened — so an author fixes them all
/// at once and only environment errors come out one at a time.
///
/// `cli_profile` is `serve -p`, which overrides the project default **and**
/// every per-cell `profile:`: a flag that silently no-ops on some cells is
/// worse than one that overrides everything, and `deploy --target` already
/// set the "CLI beats file" precedent.
pub fn load(file: &Path, cli_profile: Option<&str>) -> Result<Project> {
    let raw = std::fs::read_to_string(file)
        .with_context(|| format!("reading project file {}", file.display()))?;
    let parsed: ProjectFile = serde_yaml::from_str(&raw)
        .with_context(|| format!("parsing project file {}", file.display()))?;

    if parsed.datamk != DATAMK_PROJECT_VERSION {
        bail!(
            "`datamk: {}` in {} is a project-file version this binary does not understand \
             (supports: {DATAMK_PROJECT_VERSION}) — upgrade datamk",
            parsed.datamk,
            file.display()
        );
    }
    if parsed.cells.is_empty() {
        bail!(
            "`cells:` is empty in {} — list at least one cell path, or serve a single cell \
             with `datamk serve -f cell.yaml`",
            file.display()
        );
    }

    let dir = file.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.unwrap_or_else(|| Path::new("."));
    let default_profile = parsed.profile.as_deref().unwrap_or("local");

    let mut cells = Vec::new();
    for (index, entry) in parsed.cells.into_iter().enumerate() {
        let (path, profile, mount, no_data) = match entry {
            Entry::Path(p) => (p, None, None, false),
            Entry::Full(f) => (f.path, f.profile, f.mount, f.no_data),
        };

        // A directory means <dir>/cell.yaml; anything else is taken as the
        // definition itself, so an author can point at a differently named
        // file without a second field for it.
        let joined = dir.join(&path);
        let candidate = if joined.is_dir() {
            joined.join("cell.yaml")
        } else {
            joined
        };
        if !candidate.is_file() {
            bail!(
                "cells[{index}] `path: {path}`: no cell definition at {} — a directory means \
                 <dir>/cell.yaml; point `path:` at the .yaml file to override",
                candidate.display()
            );
        }
        // Canonicalize now — see `ProjectCell::file`.
        let file_abs = candidate
            .canonicalize()
            .with_context(|| format!("resolving cells[{index}] `path: {path}`"))?;

        if let Some(m) = &mount {
            if !valid_mount(m) {
                bail!(
                    "cells[{index}] `mount: {m}` is not a URL path segment — use \
                     [A-Za-z0-9][A-Za-z0-9_-]* (no `/`, `@`, `.`, or spaces)"
                );
            }
        }

        cells.push(ProjectCell {
            file: file_abs,
            profile: cli_profile
                .or(profile.as_deref())
                .unwrap_or(default_profile)
                .to_string(),
            mount,
            no_data,
            index,
        });
    }

    Ok(Project {
        file: file.to_path_buf(),
        cells,
    })
}

/// Validate a mount derived from a cell's own declared name. Split from
/// `valid_mount` so the error can say what the author actually has to change
/// — the fix for an illegal *derived* mount is a `mount:` override (or a
/// rename), not an edit to a field that isn't there.
pub fn check_derived_mount(cell_name: &str, cell_file: &Path, index: usize) -> Result<()> {
    if valid_mount(cell_name) {
        return Ok(());
    }
    bail!(
        "cells[{index}]: the cell name `{cell_name}` (from {}) is not a URL path segment — \
         set `mount:` to a name matching [A-Za-z0-9][A-Za-z0-9_-]*",
        cell_file.display()
    )
}

/// Reject two cells that would answer at the same URL. Runs after every cell
/// is parsed (mounts can be derived), before any router is built.
pub fn check_unique_mounts(mounts: &[(usize, String)]) -> Result<()> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for (index, mount) in mounts {
        if let Some(first) = seen.insert(mount.as_str(), *index) {
            bail!("cells[{first}] and cells[{index}] both mount at /{mount} — set `mount:` on one of them");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "datamk-project-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A cell definition thin enough to resolve — `load` never parses it.
    fn cell(dir: &Path, name: &str) {
        write(
            dir,
            &format!("{name}/cell.yaml"),
            &format!("cell: {name}\n"),
        );
    }

    #[test]
    fn shorthand_and_long_form_resolve_to_the_same_shape() {
        let d = scratch("shapes");
        cell(&d, "weather");
        cell(&d, "flight-spend");
        write(
            &d,
            "datamk.yaml",
            "datamk: 1\nprofile: prod\ncells:\n  - weather\n  - path: flight-spend\n\
             \x20   profile: local\n    mount: flights\n    no_data: true\n",
        );

        let p = load(&d.join("datamk.yaml"), None).unwrap();
        assert_eq!(p.cells.len(), 2);
        assert_eq!(p.cells[0].profile, "prod", "inherits the project default");
        assert_eq!(p.cells[0].mount, None, "shorthand derives its mount");
        assert!(!p.cells[0].no_data);
        assert_eq!(p.cells[1].profile, "local", "per-cell profile wins");
        assert_eq!(p.cells[1].mount.as_deref(), Some("flights"));
        assert!(p.cells[1].no_data);
        assert!(p.cells[0].file.is_absolute(), "canonicalized at load");
        assert!(p.cells[0].file.ends_with("weather/cell.yaml"));
    }

    /// `-p` beats the project default AND every per-cell `profile:` — the
    /// alternative is a flag that silently no-ops on some cells.
    #[test]
    fn cli_profile_overrides_every_layer() {
        let d = scratch("cliprofile");
        cell(&d, "a");
        cell(&d, "b");
        write(
            &d,
            "datamk.yaml",
            "datamk: 1\nprofile: prod\ncells:\n  - a\n  - path: b\n    profile: local\n",
        );

        let p = load(&d.join("datamk.yaml"), Some("staging")).unwrap();
        assert!(p.cells.iter().all(|c| c.profile == "staging"), "{p:?}");
    }

    #[test]
    fn a_directory_means_cell_yaml_and_a_file_is_taken_as_is() {
        let d = scratch("paths");
        cell(&d, "weather");
        write(&d, "odd/named.yaml", "cell: odd\n");
        write(
            &d,
            "datamk.yaml",
            "datamk: 1\ncells:\n  - weather\n  - odd/named.yaml\n",
        );

        let p = load(&d.join("datamk.yaml"), None).unwrap();
        assert!(p.cells[0].file.ends_with("weather/cell.yaml"));
        assert!(p.cells[1].file.ends_with("odd/named.yaml"));
    }

    #[test]
    fn authoring_errors_name_the_entry_to_edit() {
        let d = scratch("errors");
        cell(&d, "weather");

        write(&d, "v.yaml", "datamk: 2\ncells:\n  - weather\n");
        let e = load(&d.join("v.yaml"), None).unwrap_err().to_string();
        assert!(
            e.contains("`datamk: 2`") && e.contains("supports: 1"),
            "{e}"
        );

        write(&d, "empty.yaml", "datamk: 1\ncells: []\n");
        let e = load(&d.join("empty.yaml"), None).unwrap_err().to_string();
        assert!(e.contains("`cells:` is empty"), "{e}");

        write(&d, "missing.yaml", "datamk: 1\ncells:\n  - nope\n");
        let e = load(&d.join("missing.yaml"), None).unwrap_err().to_string();
        assert!(
            e.contains("cells[0]") && e.contains("no cell definition at"),
            "{e}"
        );

        write(
            &d,
            "badmount.yaml",
            "datamk: 1\ncells:\n  - path: weather\n    mount: flight spend\n",
        );
        let e = load(&d.join("badmount.yaml"), None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("is not a URL path segment"), "{e}");

        // A typo'd field is an error, not a silently ignored line.
        write(
            &d,
            "typo.yaml",
            "datamk: 1\ncells:\n  - path: weather\n    mount_point: w\n",
        );
        assert!(load(&d.join("typo.yaml"), None).is_err());
    }

    #[test]
    fn mount_grammar_excludes_what_a_url_cannot_carry() {
        assert!(valid_mount("weather"));
        assert!(valid_mount("flight-spend"));
        assert!(valid_mount("a1_b-2"));
        assert!(!valid_mount(""));
        assert!(!valid_mount("-leading"), "must start alphanumeric");
        assert!(!valid_mount("orders@1"), "@ is the export-route separator");
        assert!(!valid_mount("a/b"));
        assert!(!valid_mount("a.b"));
        assert!(!valid_mount("a b"));
        assert!(!valid_mount("a%20b"));
    }

    #[test]
    fn duplicate_mounts_are_refused_with_both_entries_named() {
        let mounts = vec![
            (0, "weather".to_string()),
            (1, "flights".to_string()),
            (2, "weather".to_string()),
        ];
        let e = check_unique_mounts(&mounts).unwrap_err().to_string();
        assert!(
            e.contains("cells[0] and cells[2] both mount at /weather"),
            "{e}"
        );
        assert!(check_unique_mounts(&mounts[..2]).is_ok());
    }

    #[test]
    fn a_derived_mount_that_is_not_url_safe_points_at_the_override() {
        let e = check_derived_mount("flight spend", Path::new("a/cell.yaml"), 1)
            .unwrap_err()
            .to_string();
        assert!(e.contains("the cell name `flight spend`"), "{e}");
        assert!(e.contains("set `mount:`"), "{e}");
        assert!(check_derived_mount("flight-spend", Path::new("a/cell.yaml"), 1).is_ok());
    }

    #[test]
    fn file_kind_dispatches_on_the_top_level_key() {
        let d = scratch("kind");
        write(&d, "datamk.yaml", "datamk: 1\ncells:\n  - a\n");
        write(&d, "cell.yaml", "cell: a\n");
        write(&d, "profiles/local.yaml", "catalog: ./x\nstorage: ./y\n");

        assert!(is_project_file(&d.join("datamk.yaml")).unwrap());
        assert!(!is_project_file(&d.join("cell.yaml")).unwrap());
        let e = is_project_file(&d.join("profiles/local.yaml"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("neither a cell definition"), "{e}");
    }
}
