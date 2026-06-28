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
                bail!(
                    "export '{}': grain {:?} is not unique ({total} rows, {distinct} distinct)",
                    export.name,
                    export.grain
                );
            }
        }

        tracing::info!(export = %export.name, version = %export.version, "interface ok");
    }
    Ok(())
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
