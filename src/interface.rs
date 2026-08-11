//! `datamk interface import` (issue #18): emit a ready-to-edit bound export
//! block from a warehouse object's own live types.
//!
//! The design point that matters most: **import emits types, never
//! descriptions.** Copying prose into `cell.yaml` IS the rot ADR 0012 §3
//! warns about, and automating that copy would make the rot cheap. Types are
//! safe to copy because `verify` checks them against the warehouse every
//! run — a stale copy is caught. Descriptions have no such check, so a copy
//! silently goes stale the moment the warehouse's own comment changes. The
//! warehouse's prose already rides `observed.source_descriptions` (issue
//! #10), live and unrottable, every time `datamk verify` runs. An author
//! writes a local `description:` only when they mean something *different*
//! from the warehouse's own words — authorship, not transcription.

use anyhow::{bail, Context, Result};
use std::path::Path;

/// Emit a bound export block for `bind` (or the cell's sole source, if
/// exactly one is declared) named `as_name` (default: the source's own
/// name). Prints the YAML block to stdout and every other byte to stderr —
/// someone will pipe this straight into `cell.yaml` on day one — unless
/// `write` is set, in which case it's spliced directly into the file's
/// `interface:` list instead.
pub fn import(
    file: &Path,
    profile: &str,
    bind: Option<&str>,
    as_name: Option<&str>,
    write: bool,
    force: bool,
) -> Result<()> {
    // Pure, offline first: resolve and validate the `--bind` name before
    // paying for a live connection at all — a typo'd `--bind` fails
    // immediately, not after a warehouse round trip.
    let loaded = crate::config::load(file, profile)?;
    let bind_name = resolve_bind_name(&loaded.def, bind)?;
    crate::verify::check_bind_target("(import)", &bind_name, &loaded.def)
        .context("the named source can't back a bound export")?;
    let as_name = as_name.unwrap_or(&bind_name).to_string();

    if !force && export_exists(&loaded.def, &as_name) {
        bail!(
            "export '{as_name}' already exists in {} — pass --force to overwrite it, or --as \
             a different name.",
            file.display()
        );
    }

    // Live: open read-only and bind every declared source (same pre-BEGIN
    // bind pass `datamk verify`'s live-check uses) to read `bind_name`'s
    // actual, current schema — the type authority is the warehouse's own
    // metadata where one exists (issue #9), DuckDB's `DESCRIBE` of the
    // bound session view otherwise (a raw file, or a connector with no
    // metadata job — not a lesser fallback, there is genuinely no other
    // authority to consult there either).
    let cell = crate::engine::open(file, profile, true)?;
    let (_, _, warehouse_columns) = crate::engine::bind_sources(&cell, false)
        .context("binding sources to read live column types for import")?;
    let actual = crate::verify::describe(&cell.conn, &bind_name)
        .with_context(|| format!("describing bound source '{bind_name}'"))?;
    let warehouse = warehouse_columns.get(&bind_name);

    let columns: Vec<ColumnType> = actual
        .iter()
        .map(|(col, actual_ty)| resolve_column_type(warehouse, col, actual_ty))
        .collect();

    let block = render_export_block(&as_name, &bind_name, &columns);

    if write {
        splice::write_export(file, &block, &as_name, force)?;
        eprintln!(
            "Wrote export '{as_name}' (bind: {bind_name}) into {}.",
            file.display()
        );
    } else {
        // The YAML block on stdout, and ONLY the YAML block — every other
        // byte (this line included) goes to stderr, so `datamk interface
        // import ... | pbcopy` (or straight into a heredoc) never has to
        // strip narration first.
        println!("{block}");
        eprintln!(
            "Paste this into {}'s `interface:` list, or re-run with --write to splice it in \
             directly.",
            file.display()
        );
    }

    let undocumented: Vec<&str> = columns
        .iter()
        .filter(|c| c.declared.is_none())
        .map(|c| c.name.as_str())
        .collect();
    if !undocumented.is_empty() {
        eprintln!(
            "{} column(s) had no datamk type name and were emitted as `type: unmapped`: {}",
            undocumented.len(),
            undocumented.join(", ")
        );
    }

    Ok(())
}

/// `--bind <name>` if given (validated against `sources:`); otherwise the
/// cell's sole declared source. Ambiguous (0 or 2+ sources, no `--bind`) is
/// a hard error naming the choice explicitly.
fn resolve_bind_name(def: &crate::config::CellDef, bind: Option<&str>) -> Result<String> {
    if let Some(b) = bind {
        if !def.sources.contains_key(b) {
            let declared: Vec<&str> = def.sources.keys().map(String::as_str).collect();
            bail!(
                "no source named '{b}' under `sources:` — declared: [{}]",
                declared.join(", ")
            );
        }
        return Ok(b.to_string());
    }
    match def.sources.len() {
        1 => Ok(def.sources.keys().next().unwrap().clone()),
        0 => bail!("cell declares no `sources:` — nothing to import"),
        n => {
            let declared: Vec<&str> = def.sources.keys().map(String::as_str).collect();
            bail!(
                "cell declares {n} sources: — pass --bind <name> to choose one: [{}]",
                declared.join(", ")
            );
        }
    }
}

fn export_exists(def: &crate::config::CellDef, name: &str) -> bool {
    def.interface.iter().any(|e| e.name == name)
}

/// One column's resolved declared type, and the type authority's own
/// string (warehouse-native when one exists for this column, DuckDB's
/// `DESCRIBE` otherwise) for the `type: unmapped` comment when there's no
/// clean declared name for it.
struct ColumnType {
    name: String,
    declared: Option<&'static str>,
    /// The authority's own type string — always populated, whether or not
    /// `declared` mapped cleanly, so the emitted comment (or a future
    /// diagnostic) can always name the real type.
    authority_ty: String,
}

/// Per column: the warehouse-native authority when this specific column has
/// one (issue #9's exact per-column dispatch, `verify::column_type_ok`
/// mirrored here for the inverse direction), DuckDB's `DESCRIBE` of the
/// bound session view otherwise.
fn resolve_column_type(
    warehouse: Option<&crate::engine::SourceWarehouseColumns>,
    col: &str,
    actual_ty: &str,
) -> ColumnType {
    if let Some(wc) = warehouse {
        if let Some((_, native_ty)) = wc.columns.iter().find(|(c, _)| c.eq_ignore_ascii_case(col)) {
            return ColumnType {
                name: col.to_string(),
                declared: crate::engine::connectors::declared_type_for(wc.connector, native_ty),
                authority_ty: native_ty.clone(),
            };
        }
    }
    ColumnType {
        name: col.to_string(),
        declared: crate::verify::duckdb_declared_type_for(actual_ty),
        authority_ty: actual_ty.to_string(),
    }
}

/// Render the export block exactly as it will be printed or spliced —
/// stdout and `--write` share this one function so what an author sees is
/// byte-identical to what lands in the file.
fn render_export_block(as_name: &str, bind_name: &str, columns: &[ColumnType]) -> String {
    let mut out = String::new();
    out.push_str(&format!("  - name: {as_name}\n"));
    out.push_str("    version: 1.0.0\n");
    out.push_str(&format!("    bind: {bind_name}\n"));
    // Commented out, deliberately: never authored automatically (the whole
    // point of this tool) — a prompt reminding the author it's theirs to
    // write, not a placeholder implying datamk tried and failed.
    out.push_str("    # description:                 # what ONE ROW means — yours to write\n");
    // Never inferred — grain is judgment about uniqueness, not a
    // transcribable fact.
    out.push_str("    grain: []                      # columns that make a row unique\n");
    out.push_str("    schema:\n");
    for col in columns {
        match col.declared {
            Some(ty) => out.push_str(&format!("      {}: {ty}\n", col.name)),
            None => {
                // Never dropped (verify only checks declared columns exist,
                // so a silent drop ships a hole in the contract) and never
                // guessed — named explicitly so the author makes the call.
                out.push_str(&format!("      {}:\n", col.name));
                out.push_str(&format!(
                    "        type: unmapped            # no datamk name for {} —\n",
                    col.authority_ty
                ));
                out.push_str(
                    "                                   # choose one, or drop the column\n",
                );
            }
        }
    }
    out.push_str("    contract: experimental\n");
    out
}

/// Byte-range textual splice into `cell.yaml`'s `interface:` list — never a
/// `serde_yaml` round-trip, which would re-serialize the whole file and
/// destroy every comment already in it (teaching ones included, and this
/// tool's own `# description:` prompt on every entry it previously wrote).
mod splice {
    use anyhow::{Context, Result};
    use std::path::Path;

    /// Insert `block` (a fully-formed, 2-space-indented `- name: ...` entry,
    /// trailing newline included) into `file`'s `interface:` list. Replaces
    /// an existing entry named `as_name` when `force` is set (the caller has
    /// already refused this case otherwise); appends when there's no
    /// existing entry, or the whole `interface:` key when the file has none
    /// at all.
    pub(super) fn write_export(file: &Path, block: &str, as_name: &str, force: bool) -> Result<()> {
        let text =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let updated = insert_or_replace(&text, block, as_name, force);
        std::fs::write(file, updated).with_context(|| format!("writing {}", file.display()))?;
        Ok(())
    }

    /// The span of `interface:`'s block-style list, as a `[start, end)` line
    /// range into `lines` (both indices exclude the `interface:` key line
    /// itself) — `start` is the first line after `interface:`, `end` is the
    /// first line at column 0 after it (a sibling top-level key) or the
    /// total line count.
    fn interface_key_line(lines: &[&str]) -> Option<usize> {
        lines.iter().position(|l| l.starts_with("interface:"))
    }

    fn is_top_level_key(line: &str) -> bool {
        !line.starts_with(' ')
            && !line.starts_with('\t')
            && !line.trim().is_empty()
            && line.contains(':')
    }

    fn block_end(lines: &[&str], start: usize) -> usize {
        lines[start..]
            .iter()
            .position(|l| is_top_level_key(l))
            .map(|off| start + off)
            .unwrap_or(lines.len())
    }

    /// The indent string (e.g. `"  "`) of the first list-item line
    /// (`<indent>- `) in `lines[start..end]`, if the block has any entries
    /// at all — `None` means an empty (or inline-`[]`) `interface:`.
    fn entry_indent(lines: &[&str], start: usize, end: usize) -> Option<String> {
        for line in &lines[start..end] {
            let trimmed = line.trim_start();
            if trimmed.starts_with("- ") {
                let indent_len = line.len() - trimmed.len();
                return Some(line[..indent_len].to_string());
            }
        }
        None
    }

    /// Line ranges `[start, end)` for every entry in `lines[list_start..
    /// list_end)`, keyed by that entry's `name:` value (read from the
    /// FIRST line of the entry, `<indent>- name: <value>` — the convention
    /// every `interface:` entry in this codebase, including this tool's own
    /// output, follows).
    fn entries(
        lines: &[&str],
        list_start: usize,
        list_end: usize,
        indent: &str,
    ) -> Vec<(String, std::ops::Range<usize>)> {
        let marker = format!("{indent}- ");
        let starts: Vec<usize> = (list_start..list_end)
            .filter(|&i| lines[i].starts_with(&marker))
            .collect();
        let mut out = Vec::with_capacity(starts.len());
        for (idx, &s) in starts.iter().enumerate() {
            let e = starts.get(idx + 1).copied().unwrap_or(list_end);
            let name = lines[s]
                .trim_start_matches(&marker)
                .strip_prefix("name:")
                .map(|rest| rest.trim().trim_matches('"').trim_matches('\'').to_string());
            if let Some(name) = name {
                out.push((name, s..e));
            }
        }
        out
    }

    fn insert_or_replace(text: &str, block: &str, as_name: &str, force: bool) -> String {
        let ends_with_newline = text.ends_with('\n');
        let lines: Vec<&str> = text.lines().collect();

        let Some(key_line) = interface_key_line(&lines) else {
            // No `interface:` key at all — append a fresh one at the end of
            // the file.
            let mut out = text.to_string();
            if !out.is_empty() && !ends_with_newline {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("interface:\n");
            out.push_str(block);
            return out;
        };

        let list_start = key_line + 1;
        let list_end = block_end(&lines, list_start);

        match entry_indent(&lines, list_start, list_end) {
            None => {
                // `interface:` exists but is empty (`interface: []`, or
                // `interface:` with nothing indented under it) — replace
                // everything from the key line through the end of its
                // (empty) span with a fresh block-style list.
                let mut out = String::new();
                for line in &lines[..key_line] {
                    out.push_str(line);
                    out.push('\n');
                }
                out.push_str("interface:\n");
                out.push_str(block);
                for line in &lines[list_end..] {
                    out.push_str(line);
                    out.push('\n');
                }
                finish(out, ends_with_newline)
            }
            Some(indent) => {
                let existing = entries(&lines, list_start, list_end, &indent);
                let collision = existing.iter().find(|(name, _)| name == as_name);

                let mut out = String::new();
                for line in &lines[..list_start] {
                    out.push_str(line);
                    out.push('\n');
                }
                for (name, range) in &existing {
                    if force && name == as_name {
                        continue; // dropped; the new block replaces it below
                    }
                    for line in &lines[range.clone()] {
                        out.push_str(line);
                        out.push('\n');
                    }
                }
                if collision.is_none() || force {
                    out.push_str(block);
                }
                for line in &lines[list_end..] {
                    out.push_str(line);
                    out.push('\n');
                }
                finish(out, ends_with_newline)
            }
        }
    }

    fn finish(mut out: String, ends_with_newline: bool) -> String {
        if !ends_with_newline && out.ends_with('\n') {
            out.pop();
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const BLOCK: &str = "  - name: qfai_customer\n    version: 1.0.0\n    bind: gold_customer\n    schema:\n      id: string\n    contract: experimental\n";

        #[test]
        fn appends_to_an_existing_interface_block() {
            let text = "cell: t\ninterface:\n  - name: existing\n    version: 1.0.0\n\naccess:\n  shareable: true\n";
            let out = insert_or_replace(text, BLOCK, "qfai_customer", false);
            assert!(out.contains("- name: existing"), "{out}");
            assert!(out.contains("- name: qfai_customer"), "{out}");
            // The new block lands before the next top-level key.
            let existing_pos = out.find("- name: existing").unwrap();
            let new_pos = out.find("- name: qfai_customer").unwrap();
            let access_pos = out.find("access:").unwrap();
            assert!(existing_pos < new_pos, "{out}");
            assert!(new_pos < access_pos, "{out}");
        }

        #[test]
        fn refuses_is_handled_by_the_caller_but_splice_itself_is_idempotent_on_no_collision() {
            // insert_or_replace has no refusal logic itself (the caller
            // checks existence first) — this just pins that a non-forced
            // call with no collision behaves like a plain append.
            let text = "cell: t\ninterface:\n  - name: existing\n    version: 1.0.0\n";
            let out = insert_or_replace(text, BLOCK, "qfai_customer", false);
            assert_eq!(out.matches("- name:").count(), 2, "{out}");
        }

        #[test]
        fn force_replaces_an_existing_entry_of_the_same_name() {
            let text = "cell: t\ninterface:\n  - name: qfai_customer\n    version: 1.0.0\n    bind: old_source\n    schema:\n      id: string\n  - name: other\n    version: 1.0.0\n";
            let out = insert_or_replace(text, BLOCK, "qfai_customer", true);
            assert_eq!(
                out.matches("- name: qfai_customer").count(),
                1,
                "must not duplicate the entry: {out}"
            );
            assert!(out.contains("bind: gold_customer"), "{out}");
            assert!(!out.contains("old_source"), "{out}");
            assert!(
                out.contains("- name: other"),
                "must not touch a sibling entry: {out}"
            );
        }

        #[test]
        fn without_force_a_collision_is_left_untouched_by_the_splice_itself() {
            // Same caveat as above: the splice function trusts the caller's
            // refusal. Given force=false and a collision, it must not
            // silently duplicate OR silently drop the existing entry.
            let text = "cell: t\ninterface:\n  - name: qfai_customer\n    version: 1.0.0\n    bind: old_source\n";
            let out = insert_or_replace(text, BLOCK, "qfai_customer", false);
            assert_eq!(out.matches("- name: qfai_customer").count(), 1, "{out}");
            assert!(
                out.contains("old_source"),
                "the untouched original must survive: {out}"
            );
        }

        #[test]
        fn creates_a_fresh_interface_block_when_none_exists() {
            let text = "cell: t\nsources:\n  raw: ./data.csv\n";
            let out = insert_or_replace(text, BLOCK, "qfai_customer", false);
            assert!(out.contains("interface:\n  - name: qfai_customer"), "{out}");
            assert!(out.contains("sources:"), "{out}");
        }

        #[test]
        fn creates_a_fresh_interface_block_when_the_file_has_no_trailing_newline() {
            let text = "cell: t";
            let out = insert_or_replace(text, BLOCK, "qfai_customer", false);
            assert!(out.contains("cell: t\n\ninterface:\n"), "{out}");
        }

        #[test]
        fn converts_an_empty_inline_interface_to_block_style() {
            let text = "cell: t\ninterface: []\naccess:\n  shareable: true\n";
            let out = insert_or_replace(text, BLOCK, "qfai_customer", false);
            assert!(out.contains("interface:\n  - name: qfai_customer"), "{out}");
            assert!(!out.contains("interface: []"), "{out}");
            assert!(out.contains("access:"), "{out}");
        }

        #[test]
        fn preserves_a_trailing_comment_on_a_sibling_entry() {
            // The exact hazard a full serde_yaml round-trip would destroy —
            // pinned directly against the splice.
            let text = "cell: t\ninterface:\n  - name: existing\n    version: 1.0.0\n    contract: supported          # teaching comment\n";
            let out = insert_or_replace(text, BLOCK, "qfai_customer", false);
            assert!(
                out.contains("contract: supported          # teaching comment"),
                "{out}"
            );
        }

        #[test]
        fn output_always_ends_with_newline_when_the_input_did() {
            let text = "cell: t\ninterface:\n  - name: existing\n    version: 1.0.0\n";
            let out = insert_or_replace(text, BLOCK, "qfai_customer", false);
            assert!(out.ends_with('\n'));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn def_with_sources(names: &[&str]) -> crate::config::CellDef {
        let mut yaml = "cell: t\nsources:\n".to_string();
        for n in names {
            yaml.push_str(&format!("  {n}: ./{n}.csv\n"));
        }
        serde_yaml::from_str(&yaml).unwrap()
    }

    #[test]
    fn resolve_bind_name_defaults_to_the_sole_source() {
        let def = def_with_sources(&["only"]);
        assert_eq!(resolve_bind_name(&def, None).unwrap(), "only");
    }

    #[test]
    fn resolve_bind_name_requires_bind_when_ambiguous() {
        let def = def_with_sources(&["a", "b"]);
        let err = resolve_bind_name(&def, None).unwrap_err().to_string();
        assert!(err.contains("--bind"), "got: {err}");
        assert!(err.contains('a') && err.contains('b'), "got: {err}");
    }

    #[test]
    fn resolve_bind_name_rejects_zero_sources() {
        let def = def_with_sources(&[]);
        let err = resolve_bind_name(&def, None).unwrap_err().to_string();
        assert!(err.contains("no `sources:`"), "got: {err}");
    }

    #[test]
    fn resolve_bind_name_rejects_an_undeclared_name() {
        let def = def_with_sources(&["only"]);
        let err = resolve_bind_name(&def, Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no source named 'nope'"), "got: {err}");
        assert!(err.contains("only"), "got: {err}");
    }

    #[test]
    fn resolve_bind_name_accepts_an_explicit_name_even_when_ambiguous() {
        let def = def_with_sources(&["a", "b"]);
        assert_eq!(resolve_bind_name(&def, Some("b")).unwrap(), "b");
    }

    #[test]
    fn export_exists_checks_the_declared_name() {
        let def: crate::config::CellDef =
            serde_yaml::from_str("cell: t\ninterface:\n  - name: existing\n    version: 1.0.0\n")
                .unwrap();
        assert!(export_exists(&def, "existing"));
        assert!(!export_exists(&def, "missing"));
    }

    fn col(name: &str, declared: Option<&'static str>, authority_ty: &str) -> ColumnType {
        ColumnType {
            name: name.to_string(),
            declared,
            authority_ty: authority_ty.to_string(),
        }
    }

    /// The exact spec shape: types as bare names, an unmapped column
    /// commented with its real type, description commented as a prompt,
    /// grain empty, contract experimental.
    #[test]
    fn render_export_block_matches_the_documented_shape() {
        let columns = vec![
            col("customer_id", Some("string"), "STRING"),
            col("credits_balance", Some("decimal"), "NUMERIC"),
            col("credits_history", None, "STRUCT<a INT64>"),
        ];
        let block = render_export_block("qfai_customer", "gold_customer", &columns);
        assert_eq!(
            block,
            "  - name: qfai_customer\n\
             \x20   version: 1.0.0\n\
             \x20   bind: gold_customer\n\
             \x20   # description:                 # what ONE ROW means — yours to write\n\
             \x20   grain: []                      # columns that make a row unique\n\
             \x20   schema:\n\
             \x20     customer_id: string\n\
             \x20     credits_balance: decimal\n\
             \x20     credits_history:\n\
             \x20       type: unmapped            # no datamk name for STRUCT<a INT64> —\n\
             \x20                                  # choose one, or drop the column\n\
             \x20   contract: experimental\n"
        );
    }

    #[test]
    fn render_export_block_never_emits_a_description_or_unit_field() {
        let columns = vec![col("id", Some("bigint"), "INT64")];
        let block = render_export_block("e", "src", &columns);
        // The `description:` field appears exactly once, and only
        // commented out — never an uncommented, authored value.
        assert_eq!(
            block.lines().filter(|l| l.contains("description:")).count(),
            1,
            "{block}"
        );
        assert!(
            block
                .lines()
                .all(|l| !l.contains("description:") || l.trim_start().starts_with('#')),
            "description: must only ever appear commented out: {block}"
        );
        assert!(!block.contains("unit:"), "{block}");
    }

    #[test]
    fn render_export_block_never_drops_an_unmapped_column() {
        let columns = vec![
            col("id", Some("bigint"), "INT64"),
            col("blob_col", None, "BYTES"),
        ];
        let block = render_export_block("e", "src", &columns);
        assert!(block.contains("id: bigint"), "{block}");
        assert!(block.contains("blob_col:"), "{block}");
        assert!(block.contains("type: unmapped"), "{block}");
    }

    #[test]
    fn resolve_column_type_prefers_the_warehouse_authority_when_present() {
        let mut cols = IndexMap::new();
        cols.insert("amount".to_string(), "NUMERIC".to_string());
        let wc = crate::engine::SourceWarehouseColumns {
            connector: "bigquery",
            columns: cols,
            descriptions: IndexMap::new(),
        };
        // DuckDB's own DESCRIBE would say VARCHAR for a wide NUMERIC (the
        // exact degradation issue #9 fixed for the check direction) — the
        // import direction must prefer the warehouse authority the same way.
        let ct = resolve_column_type(Some(&wc), "amount", "VARCHAR");
        assert_eq!(ct.declared, Some("decimal"));
        assert_eq!(ct.authority_ty, "NUMERIC");
    }

    #[test]
    fn resolve_column_type_falls_back_to_duckdb_when_the_warehouse_has_no_entry_for_this_column() {
        let wc = crate::engine::SourceWarehouseColumns {
            connector: "bigquery",
            columns: IndexMap::new(),
            descriptions: IndexMap::new(),
        };
        let ct = resolve_column_type(Some(&wc), "amount", "BIGINT");
        assert_eq!(ct.declared, Some("bigint"));
        assert_eq!(ct.authority_ty, "BIGINT");
    }

    #[test]
    fn resolve_column_type_falls_back_to_duckdb_with_no_warehouse_at_all() {
        let ct = resolve_column_type(None, "id", "VARCHAR");
        assert_eq!(ct.declared, Some("string"));
        assert_eq!(ct.authority_ty, "VARCHAR");
    }
}
