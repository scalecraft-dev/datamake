use anyhow::{bail, Context, Result};
use duckdb::Connection;
use std::collections::HashMap;
use std::path::Path;

use crate::config::{CellDef, MaterializeStrategy, ResolvedTransform};
use crate::engine;

/// ADR 0008 decision 5: for an export over a keyed table, the grain IS the
/// key. One table has one uniqueness fact, and for `upsert`/`append` the
/// field that enforces it (`key:`) is the field that states it — declaring
/// both `key:` on the transform and `grain:` on the export would be double
/// bookkeeping. So an export whose `source` resolves to an `upsert`/`append`
/// table may omit `grain:` entirely (it inherits `key:`, as row identity and
/// the filterable query params both); an explicit `grain:` there is an
/// *extension* — additional filterable columns — and must contain every key
/// column (grain may be finer than key, never coarser: a grain missing a key
/// column aliases distinct keys and cannot be unique).
///
/// Called once, from `config::load` — the single seam every consumer of
/// `def.interface` (verify, serve, openapi) shares — so `export.grain` is
/// already the effective value everywhere it's read; no downstream consumer
/// needs to know strategies exist.  Pure and offline: no DB, checked at
/// resolve time, before any connection opens.
///
/// Applies only to the **key-bearing** strategies (`append`/`upsert`). A
/// `replace` table has no `key:` (nothing to reconcile against — ADR 0008
/// decision 3), so an export sourced from one has no key to inherit:
/// `grain:` stays exactly as declared, required and runtime-checked by
/// `check` below, never auto-populated — that remains grain's load-bearing
/// role for every table this ADR doesn't hand a key to.
pub(crate) fn apply_declarative_grain_inheritance(
    def: &mut CellDef,
    transforms: &[ResolvedTransform],
) -> Result<()> {
    let keys_by_table: HashMap<&str, &[String]> = transforms
        .iter()
        .filter(|t| !matches!(t.strategy, MaterializeStrategy::Replace))
        .map(|t| (t.table.as_str(), t.key.as_slice()))
        .collect();

    for export in &mut def.interface {
        let source = export.source_object().to_string();
        let Some(&key) = keys_by_table.get(source.as_str()) else {
            continue; // raw-sourced export — grain unchanged, still required.
        };

        if export.grain.is_empty() {
            export.grain = key.to_vec();
            continue;
        }

        let missing: Vec<&String> = key
            .iter()
            .filter(|k| !export.grain.iter().any(|g| g.eq_ignore_ascii_case(k)))
            .collect();
        if !missing.is_empty() {
            bail!(
                "export '{}': grain {:?} does not contain materialize key {:?} for table \
                 '{source}' (missing {missing:?}) — grain may be finer than the key (adding \
                 filterable columns) but must never be coarser: a grain missing a key column \
                 aliases distinct keys and cannot be unique.",
                export.name,
                export.grain,
                key,
            );
        }
    }
    Ok(())
}

/// Verify a built cell against its declared interface (read-only).
pub fn run(file: &Path, profile: &str) -> Result<()> {
    let cell = engine::open(file, profile, true)?;
    check(&cell.conn, &cell.def)
}

/// The interface must not lie: every declared column must exist with a compatible
/// type, and every declared grain must exist and be unique in the actual output.
pub fn check(conn: &Connection, def: &CellDef) -> Result<()> {
    // ADR 0005 §1: `__datamk_` is a reserved, enforced namespace — a table
    // matching it other than the watermark table itself is refused before
    // publish.
    check_reserved_prefix(conn)?;
    // ADR 0005 §2 item 3: the no-grain backstop warning, fired on every
    // run/verify. It only needs the raw (unresolved) definition — whether a
    // source is incremental and whether any export declares a grain are both
    // static, contract-only facts.
    warn_no_grain_backstop(def);

    for export in &def.interface {
        let source = export.source_object();
        let actual = describe(conn, source).with_context(|| {
            format!("describing source '{source}' for export '{}'", export.name)
        })?;

        for (col, declared_ty) in &export.schema {
            match actual.iter().find(|(c, _)| c.eq_ignore_ascii_case(col)) {
                None => bail!(
                    "export '{}': declared column '{col}' missing from source '{source}'",
                    export.name
                ),
                Some((_, actual_ty)) if !type_compatible(declared_ty, actual_ty) => {
                    tracing::warn!(
                        export = %export.name, column = %col,
                        declared = %declared_ty, actual = %actual_ty,
                        "type mismatch"
                    );
                }
                Some(_) => {}
            }
        }

        for g in &export.grain {
            if !actual.iter().any(|(c, _)| c.eq_ignore_ascii_case(g)) {
                bail!(
                    "export '{}': grain column '{g}' missing from source '{source}'",
                    export.name
                );
            }
        }

        if !export.grain.is_empty() {
            let (total, distinct) = grain_counts(conn, source, &export.grain)?;
            if total != distinct {
                // ADR 0005 §2 item 5: in a cell with incremental sources, a
                // grain violation is most often a non-replay-safe transform
                // re-inserting a delta. Name that likely cause — the engine
                // cannot attribute the table to a source without parsing SQL.
                let hint = if has_incremental_source(def) {
                    " — if this table consumes an incremental source, the transform \
                     is likely not replay-safe (see docs/guides/incremental.md)"
                } else {
                    ""
                };
                bail!(
                    "export '{}': grain {:?} is not unique ({total} rows, {distinct} distinct){hint}",
                    export.name,
                    export.grain
                );
            }
        }

        tracing::info!(export = %export.name, version = %export.version, "interface ok");
    }
    Ok(())
}

/// R8: `\_\_datamk\_%` with `ESCAPE '\'` — a bare `_` is a LIKE wildcard, so
/// the naive (unescaped) pattern over-matches any two-char-then-anything
/// table name (`ab_datamk_x`). Every table matching the *escaped* pattern
/// other than `__datamk_watermarks` itself is a contract violation: the
/// prefix is engine-owned and reserved for bookkeeping, not advisory.
fn check_reserved_prefix(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_catalog = 'lake' AND table_schema = 'main' \
         AND table_name LIKE '\\_\\_datamk\\_%' ESCAPE '\\' \
         ORDER BY table_name",
    )?;
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<_, _>>()?;
    for n in names {
        if n != "__datamk_watermarks" {
            bail!(
                "verify: table '{n}' uses the reserved `__datamk_` prefix, which is \
                 engine-owned (watermarks and future bookkeeping). Rename it — only \
                 `__datamk_watermarks` may use this prefix."
            );
        }
    }
    Ok(())
}

/// Names of every source in `def` declaring `incremental:` — a static,
/// contract-only fact read from the raw definition. Shared by the no-grain
/// backstop (`sources_without_grain_backstop`, ADR 0005 §2 item 3) and the
/// `materialize: replace` incremental-cell gate (`check_replace_incremental_
/// gate`, ADR 0008 §3) — one predicate, two consumers, so they can never
/// silently drift apart on what counts as "this cell has an incremental
/// source."
fn incremental_source_names(def: &CellDef) -> Vec<&str> {
    def.sources
        .iter()
        .filter_map(|(name, src)| {
            matches!(
                src,
                crate::config::Source::Connection {
                    incremental: Some(_),
                    ..
                }
            )
            .then_some(name.as_str())
        })
        .collect()
}

/// Whether any source in the cell declares `incremental:`.
fn has_incremental_source(def: &CellDef) -> bool {
    !incremental_source_names(def).is_empty()
}

/// ADR 0008 guard 4c, resolve-time hard error: a `replace` model that
/// references an incremental source's name. `replace` rebuilds a table from
/// scratch every run with no reconciliation against prior state, so it is
/// replay-safe only when its SELECT reads a complete relation, never a
/// partial delta — and reading an incremental source directly is reading a
/// delta by definition. The engine cannot see *which* relations a model's
/// SELECT reads (decision 1: never parse the SELECT), so it can't literally
/// verify "this SELECT is safe" — but it can scan the model's file text for
/// the exact, engine-owned names that *would* make it unsafe if referenced,
/// which is metadata matching, not query comprehension. Per model, not
/// per cell (the ban this replaced): a `replace` rollup over an
/// `upsert`/`append` accumulator *in the same incremental cell* is fine —
/// it reads a complete table, not the delta — and the old cell-wide ban
/// wrongly forbade exactly that shape.
///
/// The scan is a **word-boundary token match** against source names the
/// engine owns (`sources:` keys), not SQL parsing: `events` matches `FROM
/// events e` but not `FROM fct_events` (a real accumulator table, not the
/// source) — the boundary characters on both sides of a match must be
/// non-identifier (`[^A-Za-z0-9_]` or start/end of file). Two honest edges,
/// named rather than hidden: a source name appearing in a `--` comment
/// false-positives (the fix is to remove or reword the comment — no
/// behavior changes, only text the scanner reads); indirection that evades
/// the literal token (a CTE alias, a view, string-built SQL) evades the
/// scan too and falls through to the shrink detector instead, same backstop
/// role it already plays for every other truncation risk.
///
/// **Coupling, on the record (ADR 0008 guard 4c, required at the guard
/// site):** this predicate is sound only while incremental `connection`
/// sources are the *sole* delta-producers this engine has. ADR 0005's
/// deferred incremental `Cell`/`Raw` sources do not exist yet; the day
/// either ships, `incremental_source_names` (and therefore the set of names
/// this scan matches against) MUST extend to cover them too, or the
/// founding incident reopens through exactly this gate's blind spot — a
/// `replace` model reading a not-yet-recognized incremental delta,
/// unscanned because this predicate only ever looked at `connection`
/// sources.
pub(crate) fn check_replace_incremental_gate(
    def: &CellDef,
    dir: &Path,
    transforms: &[ResolvedTransform],
) -> Result<()> {
    let incremental_sources = incremental_source_names(def);
    if incremental_sources.is_empty() {
        return Ok(());
    }
    for t in transforms {
        if !matches!(t.strategy, MaterializeStrategy::Replace) {
            continue; // upsert/append are delta consumers by design — not scanned.
        }
        let sql_path = dir.join(&t.sql);
        let text = std::fs::read_to_string(&sql_path)
            .with_context(|| format!("reading transform {}", sql_path.display()))?;
        for &source in &incremental_sources {
            if contains_word_token(&text, source) {
                bail!(
                    "transform '{}': materialize: replace references incremental source \
                     '{source}' — rebuilding from the delta would replace the table's history \
                     with just the delta (truncation). Read the accumulated table instead (an \
                     upsert/append model over '{source}' in this cell), or change this model to \
                     materialize: upsert/append if it should itself accumulate. See \
                     docs/guides/incremental.md §4.",
                    t.sql
                );
            }
        }
    }
    Ok(())
}

/// Whether `text` contains `word` as a whole token — bounded on both sides
/// by a non-identifier character (`[^A-Za-z0-9_]`) or the start/end of the
/// text. A pure string scan (ADR 0008 guard 4c: "not SQL parsing"), so it
/// makes no attempt to distinguish code from comments or string literals —
/// that imprecision is deliberate and documented at the call site.
fn contains_word_token(text: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut search_from = 0;
    while let Some(offset) = text[search_from..].find(word) {
        let start = search_from + offset;
        let end = start + word.len();
        let before_ok = text[..start].chars().next_back().is_none_or(|c| !is_ident(c));
        let after_ok = text[end..].chars().next().is_none_or(|c| !is_ident(c));
        if before_ok && after_ok {
            return true;
        }
        search_from = start + 1;
        if search_from >= text.len() {
            break;
        }
    }
    false
}

/// The incremental sources with no grain backstop (ADR 0005 §2 item 3): no
/// export anywhere in the interface declares a non-empty grain, so
/// `verify`'s grain-uniqueness check cannot catch a duplicating transform
/// for any of them. Pure — split out from `warn_no_grain_backstop` (the
/// thin logging wrapper) so this is unit-testable without a `tracing`
/// capture layer, matching this codebase's usual pure-computation/thin-I/O
/// split (`shrunk_tables`/`format_shrink_summary_lines`, `ops::
/// build_rollback_changes`/`format_rollback_lines`).
///
/// ADR 0008 decision 5 (grain): this is also the fallback a table whose
/// `materialize:` entry was removed from `transforms:` lands on. Grain
/// inheritance ends the moment that happens (`apply_declarative_grain_
/// inheritance` no longer sees a `materialize:` entry for that table), so
/// an export that relied on it and never restated its own `grain:` has
/// none — and trips this check, as long as the cell's source is a
/// detectable `incremental:` connection (the one accumulation shape this
/// check can see without parsing SQL; a purely local/file-sourced cell —
/// e.g. this repo's own `init` scaffold — gets no backstop here either way,
/// a pre-existing ADR 0005 limit this ADR does not attempt to close).
fn sources_without_grain_backstop(def: &CellDef) -> Vec<&str> {
    let has_any_grain = def.interface.iter().any(|e| !e.grain.is_empty());
    if has_any_grain {
        return Vec::new();
    }
    incremental_source_names(def)
}

fn warn_no_grain_backstop(def: &CellDef) {
    for name in sources_without_grain_backstop(def) {
        tracing::warn!(
            "incremental source '{name}' has no grain backstop: no export declares a grain, \
             so `verify` cannot catch a transform that duplicates this delta. Declare \
             `grain:` on the export, or gate CI with --verify-replay."
        );
    }
}

fn describe(conn: &Connection, source: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(&format!("DESCRIBE SELECT * FROM {source}"))?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn grain_counts(conn: &Connection, source: &str, grain: &[String]) -> Result<(i64, i64)> {
    let cols = grain.join(", ");
    let mut stmt = conn.prepare(&format!(
        "SELECT (SELECT count(*) FROM {source}) AS total,
                (SELECT count(*) FROM (SELECT DISTINCT {cols} FROM {source})) AS distinct_grain"
    ))?;
    let row = stmt.query_row([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
    Ok(row)
}

/// Loose structural compatibility between a declared type name and DuckDB's reported type.
fn type_compatible(declared: &str, actual: &str) -> bool {
    let a = actual.to_uppercase();
    match declared.to_lowercase().as_str() {
        "string" | "varchar" | "text" => a.starts_with("VARCHAR") || a == "TEXT",
        "int" | "integer" => a == "INTEGER" || a == "INT" || a == "INT32",
        "bigint" | "long" => a == "BIGINT" || a == "INT64",
        "decimal" | "numeric" => a.starts_with("DECIMAL") || a.starts_with("NUMERIC"),
        "double" | "float" => a == "DOUBLE" || a == "FLOAT" || a == "REAL",
        "bool" | "boolean" => a == "BOOLEAN",
        "date" => a == "DATE",
        "timestamp" => a.starts_with("TIMESTAMP"),
        other => a.to_lowercase() == other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // A locally ATTACHed DuckLake, mirroring engine::mod's probe helpers —
    // this file has no engine test infra of its own, so it stands up just
    // enough of a `lake` catalog to exercise `check_reserved_prefix` for
    // real, including DuckDB's own LIKE/ESCAPE semantics.
    fn attach_lake(tag: &str) -> (Connection, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "datamk-verify-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open_in_memory().expect("open in-memory duckdb");
        conn.execute_batch("INSTALL ducklake; LOAD ducklake; INSTALL json; LOAD json;")
            .expect("install/load ducklake");
        let catalog = dir.join("verify_test.ducklake");
        let data = dir.join("data");
        conn.execute_batch(&format!(
            "ATTACH 'ducklake:{}' AS lake (DATA_PATH '{}'); USE lake;",
            catalog.to_string_lossy(),
            data.to_string_lossy()
        ))
        .expect("attach ducklake");
        (conn, dir)
    }

    // R8: `_` is a LIKE wildcard — the escaped pattern must not over-match a
    // table whose name merely contains "datamk" two characters in.
    #[test]
    fn reserved_prefix_check_does_not_flag_a_two_char_prefix_match() {
        let (conn, _dir) = attach_lake("prefix-ok");
        conn.execute_batch("CREATE TABLE ab_datamk_x (id INTEGER);")
            .unwrap();
        check_reserved_prefix(&conn).expect("ab_datamk_x must not be flagged");
    }

    #[test]
    fn reserved_prefix_check_flags_an_engine_owned_prefix_collision() {
        let (conn, _dir) = attach_lake("prefix-bad");
        conn.execute_batch("CREATE TABLE __datamk_junk (id INTEGER);")
            .unwrap();
        let err = check_reserved_prefix(&conn).unwrap_err().to_string();
        assert!(err.contains("__datamk_junk"), "got: {err}");
        assert!(err.contains("reserved"), "got: {err}");
        assert!(err.contains("__datamk_watermarks"), "got: {err}");
    }

    #[test]
    fn reserved_prefix_check_ignores_the_watermark_table_itself() {
        let (conn, _dir) = attach_lake("prefix-watermarks");
        conn.execute_batch(
            "CREATE TABLE __datamk_watermarks ( \
               source VARCHAR NOT NULL, cursor_column VARCHAR NOT NULL, \
               mark_ts TIMESTAMPTZ, mark_date DATE, mark_int BIGINT, last_delta_rows BIGINT);",
        )
        .unwrap();
        check_reserved_prefix(&conn).expect("the watermark table itself must never be flagged");
    }

    // ADR 0005 §2 item 5: the grain-violation error names the likely
    // replay-safety cause — but only in a cell that declares an incremental
    // source; a plain cell keeps the plain message.
    fn grain_violation_cell(incremental: bool) -> CellDef {
        let inc = if incremental {
            "\n    incremental:\n      cursor: updated_at"
        } else {
            ""
        };
        serde_yaml::from_str(&format!(
            r#"
cell: c
sources:
  events:
    connection: crm
    table: analytics.events{inc}
interface:
  - name: dup
    version: 1.0.0
    grain: [id]
"#
        ))
        .unwrap()
    }

    fn lake_with_duplicate_grain(tag: &str) -> (Connection, PathBuf) {
        let (conn, dir) = attach_lake(tag);
        conn.execute_batch("CREATE TABLE dup AS SELECT 1 AS id UNION ALL SELECT 1;")
            .unwrap();
        (conn, dir)
    }

    #[test]
    fn grain_violation_names_the_incremental_cause_when_one_is_declared() {
        let (conn, _dir) = lake_with_duplicate_grain("grain-hint");
        let err = check(&conn, &grain_violation_cell(true))
            .unwrap_err()
            .to_string();
        assert!(err.contains("grain"), "got: {err}");
        assert!(err.contains("not replay-safe"), "got: {err}");
        assert!(err.contains("docs/guides/incremental.md"), "got: {err}");
    }

    #[test]
    fn grain_violation_stays_plain_without_incremental_sources() {
        let (conn, _dir) = lake_with_duplicate_grain("grain-plain");
        let err = check(&conn, &grain_violation_cell(false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not unique"), "got: {err}");
        assert!(!err.contains("replay-safe"), "got: {err}");
    }

    #[test]
    fn string_aliases_match_varchar() {
        assert!(type_compatible("string", "VARCHAR"));
        assert!(type_compatible("varchar", "VARCHAR(255)"));
        assert!(type_compatible("text", "TEXT"));
        assert!(!type_compatible("string", "INTEGER"));
    }

    #[test]
    fn integer_widths_are_distinguished() {
        assert!(type_compatible("int", "INTEGER"));
        assert!(type_compatible("integer", "INT32"));
        assert!(type_compatible("bigint", "BIGINT"));
        assert!(type_compatible("long", "INT64"));
        // A declared int should not silently match a bigint column.
        assert!(!type_compatible("int", "BIGINT"));
    }

    #[test]
    fn numeric_and_float_families() {
        assert!(type_compatible("decimal", "DECIMAL(18,2)"));
        assert!(type_compatible("numeric", "NUMERIC(10,0)"));
        assert!(type_compatible("double", "DOUBLE"));
        assert!(type_compatible("float", "REAL"));
    }

    #[test]
    fn temporal_and_boolean() {
        assert!(type_compatible("date", "DATE"));
        assert!(type_compatible("timestamp", "TIMESTAMP WITH TIME ZONE"));
        assert!(type_compatible("bool", "BOOLEAN"));
        assert!(type_compatible("boolean", "BOOLEAN"));
        assert!(!type_compatible("date", "TIMESTAMP"));
    }

    #[test]
    fn declared_type_matching_is_case_insensitive() {
        assert!(type_compatible("STRING", "varchar"));
        assert!(type_compatible("Integer", "INTEGER"));
    }

    #[test]
    fn unknown_declared_type_falls_back_to_case_insensitive_equality() {
        assert!(type_compatible("uuid", "UUID"));
        assert!(type_compatible("uuid", "uuid"));
        assert!(!type_compatible("uuid", "VARCHAR"));
    }

    // --- ADR 0008 Consequences: declarative grain inheritance --------------

    fn materialize_transform(table: &str, key: &[&str]) -> ResolvedTransform {
        ResolvedTransform {
            sql: format!("sql/{table}.sql"),
            strategy: crate::config::MaterializeStrategy::Upsert,
            key: key.iter().map(|s| s.to_string()).collect(),
            table: table.to_string(),
        }
    }

    fn cell_with_export(yaml_export: &str) -> CellDef {
        serde_yaml::from_str(&format!(
            "cell: t\ninterface:\n{yaml_export}\n"
        ))
        .unwrap()
    }

    #[test]
    fn omitted_grain_inherits_the_materialize_key() {
        let mut def = cell_with_export(
            "  - name: fct_flights\n    version: 1.0.0\n    source: fct_flights\n",
        );
        let transforms = vec![materialize_transform("fct_flights", &["flight_id"])];
        apply_declarative_grain_inheritance(&mut def, &transforms).unwrap();
        assert_eq!(def.interface[0].grain, vec!["flight_id".to_string()]);
    }

    #[test]
    fn omitted_grain_inherits_a_composite_materialize_key() {
        let mut def = cell_with_export(
            "  - name: fct\n    version: 1.0.0\n    source: fct\n",
        );
        let transforms = vec![materialize_transform("fct", &["a", "b"])];
        apply_declarative_grain_inheritance(&mut def, &transforms).unwrap();
        assert_eq!(def.interface[0].grain, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn export_source_defaults_to_name_for_inheritance_matching_too() {
        // `source:` omitted -> defaults to the export name (`source_object`)
        // — inheritance must resolve through that default, not require an
        // explicit `source:` restating the name.
        let mut def = cell_with_export("  - name: fct\n    version: 1.0.0\n");
        let transforms = vec![materialize_transform("fct", &["id"])];
        apply_declarative_grain_inheritance(&mut def, &transforms).unwrap();
        assert_eq!(def.interface[0].grain, vec!["id".to_string()]);
    }

    #[test]
    fn explicit_grain_extending_the_key_is_kept_as_declared() {
        let mut def = cell_with_export(
            "  - name: fct\n    version: 1.0.0\n    source: fct\n    grain: [id, region]\n",
        );
        let transforms = vec![materialize_transform("fct", &["id"])];
        apply_declarative_grain_inheritance(&mut def, &transforms).unwrap();
        assert_eq!(
            def.interface[0].grain,
            vec!["id".to_string(), "region".to_string()]
        );
    }

    #[test]
    fn explicit_grain_missing_a_key_column_errors() {
        let mut def = cell_with_export(
            "  - name: fct\n    version: 1.0.0\n    source: fct\n    grain: [region]\n",
        );
        let transforms = vec![materialize_transform("fct", &["id"])];
        let err = apply_declarative_grain_inheritance(&mut def, &transforms)
            .unwrap_err()
            .to_string();
        assert!(err.contains("export 'fct'"), "got: {err}");
        assert!(err.contains("does not contain materialize key"), "got: {err}");
        assert!(err.contains("\"id\""), "got: {err}");
        assert!(err.contains("never be coarser"), "got: {err}");
    }

    #[test]
    fn explicit_grain_missing_one_of_two_key_columns_errors() {
        let mut def = cell_with_export(
            "  - name: fct\n    version: 1.0.0\n    source: fct\n    grain: [a]\n",
        );
        let transforms = vec![materialize_transform("fct", &["a", "b"])];
        let err = apply_declarative_grain_inheritance(&mut def, &transforms)
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"b\""), "got: {err}");
    }

    #[test]
    fn export_with_no_matching_transform_table_is_left_untouched() {
        // No transform in the cell claims this export's source table at all
        // — grain (including an empty one) is exactly as declared.
        let mut def = cell_with_export("  - name: orphan_export\n    version: 1.0.0\n");
        apply_declarative_grain_inheritance(&mut def, &[]).unwrap();
        assert!(def.interface[0].grain.is_empty());
    }

    // ADR 0008 work item 4: "confirm the §2 raw-path guards are unchanged
    // and never fire on a correct declarative transform" — end to end, not
    // just at the pure-inheritance level: parse a declarative `cell.yaml`,
    // resolve its transforms, apply inheritance (mirroring `config::load`'s
    // exact sequence), then run the real, DB-connected `check()` against an
    // actual key-unique table. The grain-uniqueness backstop must pass
    // using the *inherited* grain — nothing about it is declarative-aware,
    // and it doesn't need to be.
    #[test]
    fn check_passes_for_a_declarative_export_using_inherited_grain_uniqueness() {
        let (conn, _dir) = attach_lake("declarative-grain-check");
        conn.execute_batch(
            "CREATE TABLE fct_flights (flight_id INTEGER, carrier VARCHAR); \
             INSERT INTO fct_flights VALUES (1, 'AA'), (2, 'BA');",
        )
        .unwrap();
        let mut def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             transforms:\n\
             \x20 - sql: sql/fct_flights.sql\n\
             \x20   materialize: upsert\n\
             \x20   key: [flight_id]\n\
             interface:\n\
             \x20 - name: fct_flights\n\
             \x20   version: 1.0.0\n",
        )
        .unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        apply_declarative_grain_inheritance(&mut def, &transforms).unwrap();
        assert_eq!(def.interface[0].grain, vec!["flight_id".to_string()]);

        check(&conn, &def).expect("declarative export with inherited grain must verify cleanly");
    }

    // --- ADR 0008 decision 5: the no-grain warning as the removed-entry -----
    // --- backstop -------------------------------------------------------

    #[test]
    fn a_table_with_no_materialize_entry_and_no_declared_grain_falls_back_to_the_no_grain_warning() {
        // `sources_without_grain_backstop` looks only at `def.interface`/
        // `def.sources` — it has no view of `transforms:` at all. This
        // pins the case that matters in practice: a table whose
        // `materialize:` entry was removed from `transforms:` (the table
        // now lives on in the lake, built by an earlier run, or managed
        // outside the pipeline entirely — ADR 0008 decision 8) but whose
        // export still serves it and never restated its own `grain:` — it
        // was relying on inheritance, which requires a live `materialize:`
        // entry (`apply_declarative_grain_inheritance`). It now has none,
        // which is exactly the shape `sources_without_grain_backstop` (ADR
        // 0005 §2 item 3) already exists to catch, as long as the cell's
        // source is a detectable `incremental:` connection.
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             sources:\n\
             \x20 events:\n\
             \x20   connection: crm\n\
             \x20   table: analytics.events\n\
             \x20   incremental:\n\
             \x20     cursor: updated_at\n\
             interface:\n\
             \x20 - name: fct_events\n\
             \x20   version: 1.0.0\n",
        )
        .unwrap();
        // No `transforms:` entry builds `fct_events` (removed, or never
        // this cell's to build) and the export declares no grain — never
        // restated the key it used to inherit from a live `materialize:`
        // entry.
        assert_eq!(
            sources_without_grain_backstop(&def),
            vec!["events"],
            "an export with no live `materialize:` entry to inherit grain from, and no \
             restated `grain:` of its own, must still trip the no-grain backstop when the \
             cell's source is a detectable `incremental:` connection"
        );
    }

    #[test]
    fn declaring_grain_explicitly_clears_the_no_grain_warning_with_no_materialize_entry() {
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             sources:\n\
             \x20 events:\n\
             \x20   connection: crm\n\
             \x20   table: analytics.events\n\
             \x20   incremental:\n\
             \x20     cursor: updated_at\n\
             interface:\n\
             \x20 - name: fct_events\n\
             \x20   version: 1.0.0\n\
             \x20   grain: [event_id]\n",
        )
        .unwrap();
        assert!(sources_without_grain_backstop(&def).is_empty());
    }

    #[test]
    fn a_purely_local_cell_with_no_incremental_source_has_no_grain_backstop_at_all() {
        // Documented, pre-existing limit (ADR 0005 §2 item 3), not
        // something ADR 0008 introduces or is required to close: the
        // no-grain backstop is scoped to `incremental:` connections
        // specifically — the one accumulation shape the engine can detect
        // without parsing SQL. A cell with no incremental connection at
        // all — e.g. this repo's own `init` scaffold, which reads only
        // synthesized local VALUES — gets no warning here, regardless of
        // whether an export's source used to be declarative. Pinned as a
        // known gap, not silently assumed away.
        let def: CellDef = serde_yaml::from_str(
            "cell: t\ntransforms:\n  - sql/fct.sql\ninterface:\n  - name: fct\n    version: 1.0.0\n",
        )
        .unwrap();
        assert!(sources_without_grain_backstop(&def).is_empty());
    }

    // --- ADR 0008 guard 4c: the per-model incremental-source name scan -----

    fn replace_transform(table: &str) -> ResolvedTransform {
        ResolvedTransform {
            sql: format!("sql/{table}.sql"),
            strategy: MaterializeStrategy::Replace,
            key: vec![],
            table: table.to_string(),
        }
    }

    /// A temp cell directory with `cell.yaml` and the given `sql/` files —
    /// guard 4c reads each `replace` model's actual file text (metadata
    /// scanning, never SQL parsing), so its tests need real files on disk,
    /// unlike the old cell-wide ban this replaced.
    fn gate_test_dir(tag: &str, cell_yaml: &str, sql_files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datamk-verify-gate-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sql")).unwrap();
        std::fs::write(dir.join("cell.yaml"), cell_yaml).unwrap();
        for (f, sql) in sql_files {
            std::fs::write(dir.join("sql").join(f), sql).unwrap();
        }
        dir
    }

    fn incremental_events_source_yaml() -> &'static str {
        "sources:\n\
         \x20 events:\n\
         \x20   connection: crm\n\
         \x20   table: analytics.events\n\
         \x20   incremental:\n\
         \x20     cursor: updated_at\n"
    }

    #[test]
    fn check_replace_incremental_gate_fires_on_a_replace_model_referencing_the_delta_source() {
        let dir = gate_test_dir(
            "fires",
            &format!(
                "cell: t\n{}transforms:\n  - sql: sql/rollup.sql\n    materialize: replace\n",
                incremental_events_source_yaml()
            ),
            &[("rollup.sql", "SELECT count(*) AS n FROM events")],
        );
        let def: CellDef =
            serde_yaml::from_str(&std::fs::read_to_string(dir.join("cell.yaml")).unwrap())
                .unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        let err = check_replace_incremental_gate(&def, &dir, &transforms)
            .unwrap_err()
            .to_string();
        // Exact text match — this is the gate's whole job, and the
        // coordinator asked for the implemented text to be reportable
        // verbatim, so pin it exactly rather than substring-checking.
        assert_eq!(
            err,
            "transform 'sql/rollup.sql': materialize: replace references incremental source \
             'events' — rebuilding from the delta would replace the table's history with just \
             the delta (truncation). Read the accumulated table instead (an upsert/append \
             model over 'events' in this cell), or change this model to materialize: \
             upsert/append if it should itself accumulate. See docs/guides/incremental.md §4."
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_replace_incremental_gate_is_silent_when_replace_reads_an_accumulator_in_the_same_cell(
    ) {
        // This is the case the old cell-wide ban wrongly forbade — pinned
        // per the coordinator's explicit instruction. `fct_events` contains
        // "events" as a substring but not as a word-boundary token (it's
        // preceded by `_`, an identifier character), so the accumulator
        // table itself never trips the scan.
        let dir = gate_test_dir(
            "silent-accumulator",
            &format!(
                "cell: t\n{}transforms:\n\
                 \x20 - sql: sql/fct_events.sql\n\
                 \x20   materialize: upsert\n\
                 \x20   key: [event_id]\n\
                 \x20 - sql: sql/daily_rollup.sql\n\
                 \x20   materialize: replace\n",
                incremental_events_source_yaml()
            ),
            &[
                ("fct_events.sql", "SELECT * FROM events"),
                (
                    "daily_rollup.sql",
                    "SELECT count(*) AS n FROM fct_events",
                ),
            ],
        );
        let def: CellDef =
            serde_yaml::from_str(&std::fs::read_to_string(dir.join("cell.yaml")).unwrap())
                .unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        check_replace_incremental_gate(&def, &dir, &transforms).expect(
            "a replace rollup reading an upsert accumulator's table, not the delta source \
             itself, must be legal even though the cell has an incremental source",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_replace_incremental_gate_comment_mention_is_a_documented_false_positive() {
        // The scan is metadata matching, not SQL parsing (ADR 0008 guard
        // 4c) — it cannot distinguish a comment from code. A source name
        // mentioned in a `--` comment trips the gate exactly like a real
        // reference would. This is accepted and documented, not silently
        // worked around: the fix is to delete or reword the comment (no
        // behavior changes — only text the scanner reads).
        let dir = gate_test_dir(
            "comment-false-positive",
            &format!(
                "cell: t\n{}transforms:\n  - sql: sql/rollup.sql\n    materialize: replace\n",
                incremental_events_source_yaml()
            ),
            &[(
                "rollup.sql",
                "-- TODO: eventually reconcile against events once backfilled\n\
                 SELECT count(*) AS n FROM daily_snapshot",
            )],
        );
        let def: CellDef =
            serde_yaml::from_str(&std::fs::read_to_string(dir.join("cell.yaml")).unwrap())
                .unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        let err = check_replace_incremental_gate(&def, &dir, &transforms)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("references incremental source 'events'"),
            "the comment-only mention still trips the scan (documented limitation): got {err}"
        );
        // Workaround: reword the comment so it no longer contains the bare
        // source name as a token — the SELECT itself never changes.
        std::fs::write(
            dir.join("sql/rollup.sql"),
            "-- TODO: eventually reconcile against upstream once backfilled\n\
             SELECT count(*) AS n FROM daily_snapshot",
        )
        .unwrap();
        check_replace_incremental_gate(&def, &dir, &transforms)
            .expect("rewording the comment (no code change) clears the false positive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_replace_incremental_gate_does_not_scan_upsert_or_append_models() {
        // upsert/append models are delta consumers by design — they
        // legitimately reference the source name in every real cell, so
        // the scan must never even look at their file text.
        let dir = gate_test_dir(
            "not-scanned-upsert",
            &format!(
                "cell: t\n{}transforms:\n\
                 \x20 - sql: sql/fct_events.sql\n\
                 \x20   materialize: upsert\n\
                 \x20   key: [event_id]\n",
                incremental_events_source_yaml()
            ),
            &[("fct_events.sql", "SELECT * FROM events")],
        );
        let def: CellDef =
            serde_yaml::from_str(&std::fs::read_to_string(dir.join("cell.yaml")).unwrap())
                .unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        check_replace_incremental_gate(&def, &dir, &transforms)
            .expect("upsert/append models reading the source directly are never scanned");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_replace_incremental_gate_names_multiple_incremental_sources() {
        let dir = gate_test_dir(
            "multiple-sources",
            "cell: t\n\
             sources:\n\
             \x20 events:\n\
             \x20   connection: crm\n\
             \x20   table: analytics.events\n\
             \x20   incremental:\n\
             \x20     cursor: updated_at\n\
             \x20 signups:\n\
             \x20   connection: crm\n\
             \x20   table: analytics.signups\n\
             \x20   incremental:\n\
             \x20     cursor: id\n\
             transforms:\n\
             \x20 - sql: sql/rollup.sql\n\
             \x20   materialize: replace\n",
            &[("rollup.sql", "SELECT count(*) AS n FROM signups")],
        );
        let def: CellDef =
            serde_yaml::from_str(&std::fs::read_to_string(dir.join("cell.yaml")).unwrap())
                .unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        let err = check_replace_incremental_gate(&def, &dir, &transforms)
            .unwrap_err()
            .to_string();
        assert!(err.contains("references incremental source 'signups'"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_replace_incremental_gate_is_silent_in_a_pure_derived_cell() {
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             transforms:\n\
             \x20 - sql: sql/rollup.sql\n\
             \x20   materialize: replace\n",
        )
        .unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        // No incremental source at all -> the gate returns before ever
        // reading a file, so a placeholder (nonexistent) dir is fine here.
        check_replace_incremental_gate(&def, Path::new("/nonexistent"), &transforms)
            .expect("no incremental source anywhere in the cell -> replace is legal");
    }

    #[test]
    fn check_replace_incremental_gate_is_silent_with_incremental_source_but_no_replace() {
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             sources:\n\
             \x20 events:\n\
             \x20   connection: crm\n\
             \x20   table: analytics.events\n\
             \x20   incremental:\n\
             \x20     cursor: updated_at\n\
             transforms:\n\
             \x20 - sql: sql/fct.sql\n\
             \x20   materialize: upsert\n\
             \x20   key: [id]\n",
        )
        .unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        // No `replace` entry at all -> the loop never reaches a file read
        // for any entry, so a placeholder (nonexistent) dir is fine here.
        check_replace_incremental_gate(&def, Path::new("/nonexistent"), &transforms)
            .expect("upsert alongside an incremental source is fine — only replace is scanned");
    }

    #[test]
    fn contains_word_token_matches_only_whole_tokens() {
        assert!(contains_word_token("SELECT * FROM events", "events"));
        assert!(contains_word_token("SELECT * FROM events e", "events"));
        assert!(!contains_word_token("SELECT * FROM fct_events", "events"));
        assert!(!contains_word_token("SELECT * FROM events_fct", "events"));
        assert!(!contains_word_token("SELECT * FROM other_table", "events"));
        assert!(contains_word_token("events", "events"));
        assert!(contains_word_token("(events)", "events"));
    }

    #[test]
    fn omitted_grain_over_a_replace_table_is_not_inherited() {
        let mut def =
            cell_with_export("  - name: rollup\n    version: 1.0.0\n    source: rollup\n");
        let transforms = vec![replace_transform("rollup")];
        apply_declarative_grain_inheritance(&mut def, &transforms).unwrap();
        assert!(
            def.interface[0].grain.is_empty(),
            "replace has no key to inherit — grain must stay exactly as declared (empty)"
        );
    }

    #[test]
    fn explicit_grain_over_a_replace_table_is_kept_as_declared_with_no_contains_key_check() {
        let mut def = cell_with_export(
            "  - name: rollup\n    version: 1.0.0\n    source: rollup\n    grain: [order_date, region]\n",
        );
        let transforms = vec![replace_transform("rollup")];
        apply_declarative_grain_inheritance(&mut def, &transforms).unwrap();
        assert_eq!(
            def.interface[0].grain,
            vec!["order_date".to_string(), "region".to_string()],
            "explicit grain over a replace-sourced export is untouched, exactly like raw"
        );
    }
}
