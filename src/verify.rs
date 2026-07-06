use anyhow::{bail, Context, Result};
use duckdb::Connection;
use std::path::Path;

use crate::config::CellDef;
use crate::engine;

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
                     is likely not replay-safe (see docs/incremental.md)"
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

/// ADR 0005 §2 item 3: an incremental source with no grain anywhere in the
/// interface has no uniqueness backstop — `verify`'s grain check (above)
/// cannot catch a duplicating transform if no export declares one. Warned on
/// every run/verify, not gated (attributing an export to a source would need
/// parsing the transform SQL, which this engine refuses to do).
/// Whether any source in the cell declares `incremental:` — a static,
/// contract-only fact read from the raw definition.
fn has_incremental_source(def: &CellDef) -> bool {
    def.sources.values().any(|s| {
        matches!(
            s,
            crate::config::Source::Connection {
                incremental: Some(_),
                ..
            }
        )
    })
}

fn warn_no_grain_backstop(def: &CellDef) {
    let has_any_grain = def.interface.iter().any(|e| !e.grain.is_empty());
    if has_any_grain {
        return;
    }
    for (name, src) in &def.sources {
        if let crate::config::Source::Connection {
            incremental: Some(_),
            ..
        } = src
        {
            tracing::warn!(
                "incremental source '{name}' has no grain backstop: no export declares a grain, \
                 so `verify` cannot catch a transform that duplicates this delta. Declare \
                 `grain:` on the export, or gate CI with --verify-replay."
            );
        }
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
        assert!(err.contains("docs/incremental.md"), "got: {err}");
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
}
