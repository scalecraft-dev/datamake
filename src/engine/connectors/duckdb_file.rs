//! The DuckDB-file connector: `ATTACH '<path>' (READ_ONLY)`, no extension,
//! no secret. Every object binds read-through (`ObjectKind::Table`), and —
//! unlike Postgres — the metadata is free: `duckdb_columns()` on the
//! attached database supplies native types and registered comments in one
//! local query, so classification carries both (which is what makes
//! `verify`'s type authority and the warehouse descriptions work for a
//! discovered cell on a laptop, ADR 0016 §4).

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;

use super::{quote, CursorPredicate, ObjectKind, ObjectMeta};
use crate::engine::esc;

pub(super) fn attach_sql(path: &str, alias: &str) -> String {
    format!(
        "ATTACH IF NOT EXISTS '{}' AS \"{}\" (READ_ONLY);",
        esc(path),
        quote(alias)
    )
}

pub(super) fn qualify(alias: &str, table: &str) -> Result<String> {
    let (schema, tbl) = split_schema_table(table)?;
    Ok(format!(
        "\"{}\".\"{}\".\"{}\"",
        quote(alias),
        quote(schema),
        quote(tbl)
    ))
}

/// `schema.table` — two parts, same grammar as every other connector; the
/// database is the attached file.
fn split_schema_table(table: &str) -> Result<(&str, &str)> {
    if table.contains('"') {
        bail!(
            "duckdb source table '{table}' contains a double quote — write the bare \
             `schema.table` (e.g. `main.orders`)."
        );
    }
    match table.split('.').collect::<Vec<_>>().as_slice() {
        [schema, tbl] if !schema.is_empty() && !tbl.is_empty() => Ok((schema, tbl)),
        _ => bail!(
            "duckdb source table must be `schema.table`, got '{table}' (DuckDB's default \
             schema is 'main', so a bare table name is written `main.{table}`)"
        ),
    }
}

/// One local metadata query for every table in the batch: native types
/// and registered comments from `duckdb_columns()` on the attached alias.
/// A table the file doesn't hold is an error naming it.
pub(super) fn classify_objects(
    conn: &duckdb::Connection,
    alias: &str,
    tables: &[&str],
) -> Result<IndexMap<String, ObjectMeta>> {
    let mut pairs = Vec::with_capacity(tables.len());
    for &t in tables {
        let (schema, tbl) = split_schema_table(t)?;
        pairs.push(format!("('{}', '{}')", esc(schema), esc(tbl)));
    }
    let sql = format!(
        "SELECT schema_name, table_name, column_name, data_type, comment FROM duckdb_columns() \
         WHERE database_name = '{}' AND (schema_name, table_name) IN ({}) \
         ORDER BY schema_name, table_name, column_index",
        esc(alias),
        pairs.join(", ")
    );
    let mut stmt = conn
        .prepare(&sql)
        .context("reading duckdb_columns() for the attached duckdb file")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut out: IndexMap<String, ObjectMeta> = IndexMap::new();
    for row in rows {
        let (schema, table, col, ty, comment) = row?;
        let entry = out
            .entry(format!("{schema}.{table}"))
            .or_insert_with(|| ObjectMeta {
                kind: ObjectKind::Table,
                columns: IndexMap::new(),
                descriptions: IndexMap::new(),
            });
        entry.columns.insert(col.clone(), ty);
        if let Some(c) = comment.filter(|c| !c.trim().is_empty()) {
            entry.descriptions.insert(col, c);
        }
    }
    for &t in tables {
        if !out.contains_key(t) {
            bail!(
                "duckdb table '{t}' was not found in the attached file — check the table \
                 exists (schema.table) and the path points at the right database"
            );
        }
    }
    Ok(out)
}

pub(super) fn read_sql(
    alias: &str,
    table: &str,
    predicate: Option<&CursorPredicate>,
) -> Result<String> {
    let qualified = qualify(alias, table)?;
    Ok(match predicate {
        Some(p) => {
            let cq = p.cursor.replace('"', "\"\"");
            format!(
                "SELECT * FROM {qualified} WHERE \"{cq}\" > {}",
                p.mark.as_literal()
            )
        }
        None => format!("SELECT * FROM {qualified}"),
    })
}
