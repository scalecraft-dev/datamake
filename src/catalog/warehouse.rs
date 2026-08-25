//! The warehouse read that supplies column definitions for discovered
//! models (ADR 0016 §4): types for every model, descriptions for what the
//! tool registered on the object (`register_comments`). Batched per
//! `(connection, schema)` — K round trips for N models — and run only by
//! `datamk sync`, never by a serving process.

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;

use crate::config::ResolvedConnection;

/// Where the discovered objects live: a profile `connections:` entry —
/// bigquery, postgres, or a `type: duckdb` file (the SQLMesh default
/// engine, which also holds the state store in the fixture project).
#[derive(Debug, Clone)]
pub struct Warehouse {
    pub name: String,
    pub resolved: ResolvedConnection,
}

/// One object's column definitions, as the warehouse reports them.
#[derive(Debug, Clone, Default)]
pub struct WarehouseColumns {
    /// column -> warehouse-native type, in ordinal order.
    pub columns: IndexMap<String, String>,
    /// column -> registered comment, non-empty only.
    pub descriptions: IndexMap<String, String>,
    /// The object's own registered comment, if any.
    pub table_description: Option<String>,
}

use crate::engine::connectors::DUCKDB_CLASSIFY_ALIAS as ALIAS;

fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// One deployed object to read: its catalog (the tool's, e.g. a BigQuery
/// project — `None` for a two-part model name), schema and table.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObjectRef {
    pub catalog: Option<String>,
    pub schema: String,
    pub table: String,
}

impl ObjectRef {
    pub fn object(&self) -> String {
        format!("{}.{}", self.schema, self.table)
    }
}

/// `objects` grouped by the catalog they live in, in first-seen order —
/// `None` (a two-part name) reads from the connection's own catalog.
pub fn by_catalog(objects: &[ObjectRef]) -> IndexMap<Option<String>, Vec<&ObjectRef>> {
    let mut groups: IndexMap<Option<String>, Vec<&ObjectRef>> = IndexMap::new();
    for o in objects {
        groups.entry(o.catalog.clone()).or_default().push(o);
    }
    groups
}

/// Read every object in `objects` from `warehouse`, keyed by
/// `schema.table`. An object the warehouse doesn't know is simply absent
/// from the result — the caller decides whether that is an error
/// (`on_unresolvable`).
///
/// Catalogs: BigQuery reads each object from its own project (one
/// connection, many projects — a modeling project's catalogs); a Postgres
/// or DuckDB connection is one database, and an object whose catalog is
/// not that database is absent, with a warning naming it.
pub fn read_columns(
    conn: &duckdb::Connection,
    warehouse: &Warehouse,
    objects: &[ObjectRef],
) -> Result<IndexMap<String, WarehouseColumns>> {
    if objects.is_empty() {
        return Ok(IndexMap::new());
    }
    let Warehouse { name, resolved } = warehouse;
    match resolved {
        ResolvedConnection::Duckdb { .. } => {
            conn.execute_batch(&resolved.attach_sql(ALIAS))
                .map_err(|e| resolved.rewrite_attach_error(e, name))?;
            // A file is one catalog; every object's catalog is that file
            // under whatever name the tool gave it.
            read_duckdb_file(conn, &pairs(objects))
        }
        ResolvedConnection::Bigquery {
            credentials,
            project,
            ..
        } => {
            conn.execute_batch(resolved.install_load_sql())
                .with_context(|| {
                    format!("loading the bigquery extension for connection '{name}'")
                })?;
            if let Some(path) = credentials {
                resolved.point_credentials_at(path);
            }
            let mut out = IndexMap::new();
            for (catalog, group) in by_catalog(objects) {
                let target = catalog.as_deref().unwrap_or(project);
                let refs: Vec<String> = group.iter().map(|o| o.object()).collect();
                let ref_strs: Vec<&str> = refs.iter().map(String::as_str).collect();
                let meta = resolved
                    .classify_objects_in_project(conn, target, &ref_strs)
                    .with_context(|| {
                        format!(
                            "reading BigQuery INFORMATION_SCHEMA in project '{target}' for \
                             discovered models (connection '{name}')"
                        )
                    })?;
                for (object, m) in meta {
                    out.insert(
                        object,
                        WarehouseColumns {
                            columns: m.columns,
                            descriptions: m.descriptions,
                            table_description: None,
                        },
                    );
                }
            }
            Ok(out)
        }
        ResolvedConnection::Postgres { database, .. } => {
            conn.execute_batch(resolved.install_load_sql())
                .with_context(|| {
                    format!("loading the postgres extension for connection '{name}'")
                })?;
            conn.execute_batch(&resolved.attach_sql(ALIAS))
                .map_err(|e| resolved.rewrite_attach_error(e, name))?;
            let (reachable, foreign): (Vec<&ObjectRef>, Vec<&ObjectRef>) = objects
                .iter()
                .partition(|o| o.catalog.as_deref().is_none_or(|c| c == database));
            if !foreign.is_empty() {
                tracing::warn!(
                    objects = ?foreign.iter().map(|o| o.object()).collect::<Vec<_>>(),
                    "connection '{name}' reads database '{database}' only; these models live in \
                     another catalog and cannot be resolved through it"
                );
            }
            let reachable: Vec<ObjectRef> = reachable.into_iter().cloned().collect();
            read_postgres(conn, &pairs(&reachable))
        }
        ResolvedConnection::Snowflake { .. } => bail!(
            "connection '{name}' is Snowflake: reading discovered models' column \
             definitions from Snowflake is not supported yet (ADR 0016 §4 names it as \
             same-shaped work) — point `discover.warehouse` at a bigquery, postgres, or \
             duckdb connection, or declare `columns` on the models"
        ),
    }
}

fn read_duckdb_file(
    conn: &duckdb::Connection,
    objects: &[(String, String)],
) -> Result<IndexMap<String, WarehouseColumns>> {
    let pairs: Vec<String> = objects
        .iter()
        .map(|(s, t)| format!("({}, {})", q(s), q(t)))
        .collect();
    let in_list = pairs.join(", ");
    let mut out: IndexMap<String, WarehouseColumns> = IndexMap::new();
    let sql = format!(
        "SELECT schema_name, table_name, column_name, data_type, comment FROM duckdb_columns() \
         WHERE database_name = '{ALIAS}' AND (schema_name, table_name) IN ({in_list}) \
         ORDER BY schema_name, table_name, column_index"
    );
    let mut stmt = conn.prepare(&sql).context("reading duckdb_columns()")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    })?;
    for row in rows {
        let (schema, table, col, ty, comment) = row?;
        let entry = out.entry(format!("{schema}.{table}")).or_default();
        entry.columns.insert(col.clone(), ty);
        if let Some(c) = comment.filter(|c| !c.trim().is_empty()) {
            entry.descriptions.insert(col, c);
        }
    }
    // Object comments: tables and views each report theirs.
    for (fn_name, name_col) in [
        ("duckdb_tables", "table_name"),
        ("duckdb_views", "view_name"),
    ] {
        let sql = format!(
            "SELECT schema_name, {name_col}, comment FROM {fn_name}() \
             WHERE database_name = '{ALIAS}' AND (schema_name, {name_col}) IN ({in_list})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (schema, table, comment) = row?;
            if let Some(entry) = out.get_mut(&format!("{schema}.{table}")) {
                entry.table_description = comment.filter(|c| !c.trim().is_empty());
            }
        }
    }
    Ok(out)
}

fn read_postgres(
    conn: &duckdb::Connection,
    objects: &[(String, String)],
) -> Result<IndexMap<String, WarehouseColumns>> {
    // One server-side query per sync: `information_schema.columns` plus
    // `col_description`/`obj_description` for the registered comments.
    // Identifiers are compared as data, never spliced as identifiers.
    let pairs: Vec<String> = objects
        .iter()
        .map(|(s, t)| format!("({}, {})", q(s), q(t)))
        .collect();
    let in_list = pairs.join(", ");
    let inner = format!(
        "SELECT c.table_schema, c.table_name, c.column_name, c.data_type, \
         pg_catalog.col_description(pc.oid, c.ordinal_position) AS column_comment, \
         pg_catalog.obj_description(pc.oid, 'pg_class') AS table_comment, c.ordinal_position \
         FROM information_schema.columns c \
         JOIN pg_catalog.pg_namespace pn ON pn.nspname = c.table_schema \
         JOIN pg_catalog.pg_class pc ON pc.relnamespace = pn.oid AND pc.relname = c.table_name \
         WHERE (c.table_schema, c.table_name) IN ({in_list}) \
         ORDER BY c.table_schema, c.table_name, c.ordinal_position"
    );
    let sql = format!("SELECT * FROM postgres_query('{ALIAS}', {})", q(&inner));
    let mut stmt = conn
        .prepare(&sql)
        .context("reading Postgres information_schema for discovered models")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut out: IndexMap<String, WarehouseColumns> = IndexMap::new();
    for row in rows {
        let (schema, table, col, ty, col_comment, table_comment) = row?;
        let entry = out.entry(format!("{schema}.{table}")).or_default();
        entry.columns.insert(col.clone(), ty);
        if let Some(c) = col_comment.filter(|c| !c.trim().is_empty()) {
            entry.descriptions.insert(col, c);
        }
        if entry.table_description.is_none() {
            entry.table_description = table_comment.filter(|c| !c.trim().is_empty());
        }
    }
    Ok(out)
}

fn pairs(objects: &[ObjectRef]) -> Vec<(String, String)> {
    objects
        .iter()
        .map(|o| (o.schema.clone(), o.table.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objects_group_by_catalog_in_first_seen_order() {
        let o = |c: Option<&str>, s: &str, t: &str| ObjectRef {
            catalog: c.map(str::to_string),
            schema: s.to_string(),
            table: t.to_string(),
        };
        let objects = vec![
            o(Some("dw-main-silver"), "invoice", "flight_spend"),
            o(Some("dw-main-gold"), "ddm", "metrics"),
            o(Some("dw-main-silver"), "ui", "ui_flights"),
            o(None, "public", "x"),
        ];
        let groups = by_catalog(&objects);
        let keys: Vec<Option<String>> = groups.keys().cloned().collect();
        assert_eq!(
            keys,
            vec![
                Some("dw-main-silver".to_string()),
                Some("dw-main-gold".to_string()),
                None
            ]
        );
        assert_eq!(groups[&Some("dw-main-silver".to_string())].len(), 2);
    }
}
