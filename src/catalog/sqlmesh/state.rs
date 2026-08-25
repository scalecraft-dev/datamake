//! Reading the SQLMesh state store (ADR 0016 §3) through a DuckDB
//! connection on which the store is reachable as `<alias>.<schema>.<table>`
//! — a Postgres ATTACH, a DuckDB/SQLite file ATTACH, or (in tests) tables
//! loaded into memory. Exactly four reads, all `SELECT`s:
//!
//! 1. `_versions` — one row; `schema_version` must be one this reader was
//!    tested against.
//! 2. `_environments WHERE name = ?` — one row; refused when unfinalized.
//! 3. `_snapshots WHERE identifier IN (…)` — the environment's snapshots,
//!    joined on `(name, crc32(fingerprint))`, never on `name` alone
//!    (`(name, version)` is not unique; see `identifier`).
//! 4. `_intervals` for those `(name, version)` pairs.
//!
//! Every blob field is read as `Option` through a `serde_json::Value` walk
//! with a named error per required field — SQLMesh's blob shape moves
//! between minor releases without a `schema_version` bump, and a typed
//! struct with required fields over another tool's private JSON would fail
//! on the wrong day.

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use serde_json::Value;

use super::identifier::Fingerprint;
use super::names::{self, ModelName};

/// The `_versions.schema_version` values this reader has been exercised
/// against. A different one is a hard error naming both numbers (§3).
pub const SUPPORTED_SCHEMA_VERSIONS: &[i64] = &[100];

#[derive(Debug, Clone)]
pub struct Versions {
    pub schema_version: i64,
    pub sqlmesh_version: String,
}

#[derive(Debug, Clone)]
pub struct Environment {
    pub name: String,
    pub plan_id: String,
    /// Epoch millis; `None` while an apply is between promote and finalize.
    pub finalized_ts: Option<i64>,
    pub catalog_name_override: Option<String>,
    /// `(name as stored, fingerprint, version)` per promoted snapshot.
    pub snapshots: Vec<EnvSnapshot>,
}

#[derive(Debug, Clone)]
pub struct EnvSnapshot {
    pub raw_name: String,
    pub fingerprint: Fingerprint,
    pub version: String,
}

/// One deployed model, as far as the state store alone can describe it.
/// Column types/descriptions here are only what the model **declared**
/// (`columns`/`column_descriptions`) plus inline comments; the warehouse
/// fills the rest (`catalog::warehouse`).
#[derive(Debug, Clone)]
pub struct StateModel {
    pub name: ModelName,
    pub raw_name: String,
    pub identifier: String,
    pub version: String,
    pub data_hash: String,
    pub kind: String,
    pub cron: Option<String>,
    pub owner: Option<String>,
    pub tags: Vec<String>,
    pub description: Option<String>,
    /// Declared `columns` (name -> type), in declared order. Empty when the
    /// model declares none — the common case for SQL models.
    pub declared_columns: IndexMap<String, String>,
    /// `column_descriptions`: declared in `MODEL(...)` if present, else
    /// derived from the query's inline comments (`comments`), the way
    /// SQLMesh itself does.
    pub column_descriptions: IndexMap<String, String>,
    pub grain: Vec<String>,
    /// Parents' names, unquoted.
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct IntervalRow {
    pub start_ts: i64,
    pub end_ts: i64,
    pub pending_restatement: bool,
}

/// `"<alias>"."<schema>"."<table>"` — every identifier double-quoted with
/// `"` doubled, so an alias or schema an operator typed can't break the SQL.
fn qualified(alias: &str, schema: &str, table: &str) -> String {
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    format!("{}.{}.{}", q(alias), q(schema), q(table))
}

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn read_versions(conn: &duckdb::Connection, alias: &str, schema: &str) -> Result<Versions> {
    let sql = format!(
        "SELECT schema_version, sqlmesh_version FROM {}",
        qualified(alias, schema, "_versions")
    );
    let mut stmt = conn.prepare(&sql).with_context(|| {
        format!("reading the SQLMesh state store's _versions ({schema}._versions)")
    })?;
    let rows: Vec<Versions> = stmt
        .query_map([], |r| {
            Ok(Versions {
                schema_version: r.get::<_, i64>(0)?,
                sqlmesh_version: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            })
        })
        .context("reading _versions")?
        .collect::<std::result::Result<_, _>>()?;
    match rows.len() {
        1 => Ok(rows.into_iter().next().unwrap()),
        0 => bail!(
            "the SQLMesh state store at {schema} has no _versions row — has `sqlmesh plan` \
             ever run against it?"
        ),
        n => bail!("the SQLMesh state store at {schema} has {n} _versions rows; expected one"),
    }
}

/// The pin-and-fail-loud check (§3).
pub fn check_schema_version(v: &Versions) -> Result<()> {
    if !SUPPORTED_SCHEMA_VERSIONS.contains(&v.schema_version) {
        bail!(
            "unsupported SQLMesh state schema_version {} (this datamk supports {}; the store \
             was written by sqlmesh {}). datamk reads the state tables directly, so a schema \
             change can silently change what \"deployed\" means — upgrade datamk, or pin \
             SQLMesh to a supported version.",
            v.schema_version,
            SUPPORTED_SCHEMA_VERSIONS
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            v.sqlmesh_version
        );
    }
    Ok(())
}

fn fingerprint_of(v: &Value, what: &str) -> Result<Fingerprint> {
    let get = |k: &str| -> Result<String> {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .with_context(|| format!("{what}: fingerprint has no `{k}`"))
    };
    Ok(Fingerprint {
        data_hash: get("data_hash")?,
        metadata_hash: get("metadata_hash")?,
        parent_data_hash: get("parent_data_hash")?,
        parent_metadata_hash: get("parent_metadata_hash")?,
    })
}

pub fn read_environment(
    conn: &duckdb::Connection,
    alias: &str,
    schema: &str,
    environment: &str,
) -> Result<Environment> {
    let sql = format!(
        "SELECT name, plan_id, finalized_ts, catalog_name_override, snapshots FROM {} \
         WHERE name = {}",
        qualified(alias, schema, "_environments"),
        sql_str(environment)
    );
    let mut stmt = conn
        .prepare(&sql)
        .context("reading the SQLMesh state store's _environments")?;
    type EnvRow = (
        String,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<EnvRow> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .context("reading _environments")?
        .collect::<std::result::Result<_, _>>()?;
    let Some((name, plan_id, finalized_ts, catalog_name_override, snapshots)) =
        rows.into_iter().next()
    else {
        let names = list_environments(conn, alias, schema).unwrap_or_default();
        bail!(
            "no environment named '{environment}' in the SQLMesh state store. Environments \
             present: {}. (SQLMesh creates an environment on the first `sqlmesh plan` against \
             it — an unplanned project has none.)",
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            }
        );
    };
    let plan_id = plan_id.with_context(|| format!("environment '{environment}' has no plan_id"))?;
    let list: Value = serde_json::from_str(snapshots.as_deref().unwrap_or("[]"))
        .with_context(|| format!("environment '{environment}': `snapshots` is not JSON"))?;
    let Some(entries) = list.as_array() else {
        bail!("environment '{environment}': `snapshots` is not a JSON list");
    };
    let mut out = Vec::with_capacity(entries.len());
    for (i, e) in entries.iter().enumerate() {
        let what = format!("environment '{environment}' snapshot #{i}");
        let raw_name = e
            .get("name")
            .and_then(|v| v.as_str())
            .with_context(|| format!("{what}: no `name`"))?
            .to_string();
        let fingerprint = fingerprint_of(
            e.get("fingerprint")
                .with_context(|| format!("{what} ({raw_name}): no `fingerprint`"))?,
            &what,
        )?;
        let version = e
            .get("version")
            .and_then(|v| v.as_str())
            .with_context(|| format!("{what} ({raw_name}): no `version`"))?
            .to_string();
        out.push(EnvSnapshot {
            raw_name,
            fingerprint,
            version,
        });
    }
    Ok(Environment {
        name,
        plan_id,
        finalized_ts,
        catalog_name_override: catalog_name_override.filter(|s| !s.is_empty()),
        snapshots: out,
    })
}

fn list_environments(conn: &duckdb::Connection, alias: &str, schema: &str) -> Result<Vec<String>> {
    let sql = format!(
        "SELECT name FROM {} ORDER BY name",
        qualified(alias, schema, "_environments")
    );
    let mut stmt = conn.prepare(&sql)?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(names)
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// A grain expression as SQLMesh stores it: `item_id` or `(id, event_date)`
/// — one or many columns.
fn grain_columns(v: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    for g in str_list(v) {
        let inner = g.trim().trim_start_matches('(').trim_end_matches(')');
        for col in inner.split(',') {
            let col = col.trim().trim_matches('"').trim_matches('`');
            if !col.is_empty() {
                out.push(col.to_string());
            }
        }
    }
    out
}

/// Every environment snapshot's `_snapshots` row, joined on
/// `(name, identifier)`. Missing rows are an error naming the model — the
/// environment promised a snapshot the store no longer holds.
pub fn read_models(
    conn: &duckdb::Connection,
    alias: &str,
    schema: &str,
    env: &Environment,
) -> Result<Vec<StateModel>> {
    if env.snapshots.is_empty() {
        return Ok(Vec::new());
    }
    let wanted: IndexMap<(String, String), &EnvSnapshot> = env
        .snapshots
        .iter()
        .map(|s| ((s.raw_name.clone(), s.fingerprint.identifier()), s))
        .collect();
    let ids: Vec<String> = wanted.keys().map(|(_, id)| sql_str(id)).collect();
    let sql = format!(
        "SELECT name, identifier, version, snapshot, kind_name FROM {} \
         WHERE identifier IN ({})",
        qualified(alias, schema, "_snapshots"),
        ids.join(", ")
    );
    let mut stmt = conn
        .prepare(&sql)
        .context("reading the SQLMesh state store's _snapshots")?;
    type SnapshotRow = (String, String, Option<String>, String, Option<String>);
    let rows: Vec<SnapshotRow> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .context("reading _snapshots")?
        .collect::<std::result::Result<_, _>>()?;

    let mut by_key: IndexMap<(String, String), StateModel> = IndexMap::new();
    for (raw_name, identifier, version, blob, kind_name) in rows {
        let key = (raw_name.clone(), identifier.clone());
        let Some(env_snapshot) = wanted.get(&key) else {
            continue; // a same-identifier row under another name: not ours
        };
        let snapshot: Value = serde_json::from_str(&blob).with_context(|| {
            format!("snapshot {raw_name} ({identifier}): `snapshot` is not JSON")
        })?;
        let node = snapshot
            .get("node")
            .with_context(|| format!("snapshot {raw_name}: no `node`"))?;
        let mut name = names::parse(&raw_name)?;
        // `catalog_name_override` (an environment-wide catalog substitution)
        // applies to every model's virtual object in that environment.
        if let Some(c) = &env.catalog_name_override {
            name.catalog = Some(c.clone());
        }
        let data_hash = snapshot
            .get("fingerprint")
            .and_then(|f| f.get("data_hash"))
            .and_then(|v| v.as_str())
            .unwrap_or(&env_snapshot.fingerprint.data_hash)
            .to_string();
        let kind = node
            .get("kind")
            .and_then(|k| k.get("name"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or(kind_name)
            .with_context(|| format!("snapshot {raw_name}: no `kind`"))?;
        let dialect = node
            .get("dialect")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let declared_columns: IndexMap<String, String> = node
            .get("columns")
            .and_then(|c| c.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|t| (k.clone(), t.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let column_descriptions: IndexMap<String, String> =
            match node.get("column_descriptions").and_then(|c| c.as_object()) {
                Some(m) => m
                    .iter()
                    .filter_map(|(k, v)| v.as_str().map(|t| (k.clone(), t.to_string())))
                    .collect(),
                None => node
                    .get("query")
                    .and_then(|q| q.get("sql"))
                    .and_then(|v| v.as_str())
                    .map(|sql| super::comments::column_descriptions(sql, &dialect))
                    .unwrap_or_default(),
            };
        let depends_on = snapshot
            .get("parents")
            .and_then(|p| p.as_array())
            .map(|ps| {
                ps.iter()
                    .filter_map(|p| p.get("name").and_then(|v| v.as_str()))
                    .filter_map(|n| names::parse(n).ok().map(|m| m.unquoted()))
                    .collect()
            })
            .unwrap_or_default();
        let model = StateModel {
            name,
            raw_name: raw_name.clone(),
            identifier: identifier.clone(),
            version: version.unwrap_or_else(|| env_snapshot.version.clone()),
            data_hash,
            kind,
            cron: node
                .get("cron")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            owner: node
                .get("owner")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            tags: str_list(node.get("tags")),
            description: node
                .get("description")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string),
            declared_columns,
            column_descriptions,
            grain: grain_columns(node.get("grains")),
            depends_on,
        };
        by_key.insert(key, model);
    }

    let mut out = Vec::with_capacity(env.snapshots.len());
    for s in &env.snapshots {
        let key = (s.raw_name.clone(), s.fingerprint.identifier());
        match by_key.shift_remove(&key) {
            Some(m) => out.push(m),
            None => bail!(
                "environment '{}' names snapshot {} (identifier {}) but _snapshots holds no such \
                 row — the state store is inconsistent, or was compacted underneath the \
                 environment",
                env.name,
                s.raw_name,
                key.1
            ),
        }
    }
    Ok(out)
}

/// Loaded intervals per `(raw_name, version)`, non-dev and non-removed,
/// rolled up to `[min start, max end)` and whether any is pending
/// restatement.
pub fn read_intervals(
    conn: &duckdb::Connection,
    alias: &str,
    schema: &str,
    models: &[StateModel],
) -> Result<IndexMap<String, IntervalRow>> {
    let mut out = IndexMap::new();
    if models.is_empty() {
        return Ok(out);
    }
    let pairs: Vec<String> = models
        .iter()
        .map(|m| format!("({}, {})", sql_str(&m.raw_name), sql_str(&m.version)))
        .collect();
    let sql = format!(
        "SELECT name, min(start_ts), max(end_ts), bool_or(is_pending_restatement) FROM {} \
         WHERE NOT is_dev AND NOT is_removed AND (name, version) IN ({}) GROUP BY name",
        qualified(alias, schema, "_intervals"),
        pairs.join(", ")
    );
    let mut stmt = conn
        .prepare(&sql)
        .context("reading the SQLMesh state store's _intervals")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<bool>>(3)?,
            ))
        })
        .context("reading _intervals")?;
    for row in rows {
        let (name, start, end, pending) = row?;
        if let (Some(start_ts), Some(end_ts)) = (start, end) {
            out.insert(
                name,
                IntervalRow {
                    start_ts,
                    end_ts,
                    pending_restatement: pending.unwrap_or(false),
                },
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod fixture {
    //! The checked-in state tables (`test/fixtures/sqlmesh/state/*.jsonl`,
    //! exported from a `sqlmesh init` duckdb project with prod + a dev
    //! environment) loaded into an in-memory DuckDB under schema `sqlmesh`
    //! of the default catalog `memory` — so the reader runs the exact SQL
    //! it runs against an ATTACHed store.

    pub fn connection() -> duckdb::Connection {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        load_state_tables(&conn);
        conn
    }

    /// A state store **file** plus the virtual-layer objects the fixture
    /// project's `prod` environment exposes (`sqlmesh_example.*`, with the
    /// column/object comments SQLMesh registered) — what a `type: duckdb`
    /// profile connection points at, so `sync`, `verify` and `context` run
    /// end to end against a real database on disk.
    pub fn build_file(path: &std::path::Path) {
        let conn = duckdb::Connection::open(path).unwrap();
        load_state_tables(&conn);
        conn.execute_batch("CREATE SCHEMA sqlmesh_example;")
            .unwrap();
        let base = concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixtures/sqlmesh/state/");
        let cols: Vec<serde_json::Value> =
            std::fs::read_to_string(format!("{base}warehouse_columns.jsonl"))
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
        let objects: Vec<serde_json::Value> =
            std::fs::read_to_string(format!("{base}warehouse_objects.jsonl"))
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
        let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let lit = |s: &str| format!("'{}'", s.replace('\'', "''"));
        for o in &objects {
            let table = o["table"].as_str().unwrap();
            let defs: Vec<String> = cols
                .iter()
                .filter(|c| c["table"] == table)
                .map(|c| {
                    format!(
                        "{} {}",
                        q(c["column"].as_str().unwrap()),
                        c["type"].as_str().unwrap()
                    )
                })
                .collect();
            conn.execute_batch(&format!(
                "CREATE TABLE sqlmesh_example.{} ({});",
                q(table),
                defs.join(", ")
            ))
            .unwrap();
            if let Some(comment) = o["comment"].as_str() {
                conn.execute_batch(&format!(
                    "COMMENT ON TABLE sqlmesh_example.{} IS {};",
                    q(table),
                    lit(comment)
                ))
                .unwrap();
            }
            for c in cols.iter().filter(|c| c["table"] == table) {
                if let Some(comment) = c["comment"].as_str() {
                    conn.execute_batch(&format!(
                        "COMMENT ON COLUMN sqlmesh_example.{}.{} IS {};",
                        q(table),
                        q(c["column"].as_str().unwrap()),
                        lit(comment)
                    ))
                    .unwrap();
                }
            }
        }
    }

    fn load_state_tables(conn: &duckdb::Connection) {
        conn.execute_batch("CREATE SCHEMA sqlmesh;").unwrap();
        let base = concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixtures/sqlmesh/state/");
        let typed = [
            (
                "_versions",
                "{schema_version: 'BIGINT', sqlglot_version: 'VARCHAR', sqlmesh_version: 'VARCHAR'}",
            ),
            (
                "_environments",
                "{name: 'VARCHAR', snapshots: 'VARCHAR', start_at: 'VARCHAR', end_at: 'VARCHAR', \
                 plan_id: 'VARCHAR', previous_plan_id: 'VARCHAR', expiration_ts: 'BIGINT', \
                 finalized_ts: 'BIGINT', promoted_snapshot_ids: 'VARCHAR', suffix_target: 'VARCHAR', \
                 catalog_name_override: 'VARCHAR', previous_finalized_snapshots: 'VARCHAR', \
                 normalize_name: 'BOOLEAN', requirements: 'VARCHAR', gateway_managed: 'BOOLEAN'}",
            ),
            (
                "_snapshots",
                "{name: 'VARCHAR', identifier: 'VARCHAR', version: 'VARCHAR', snapshot: 'VARCHAR', \
                 kind_name: 'VARCHAR', updated_ts: 'BIGINT', unpaused_ts: 'BIGINT', ttl_ms: 'BIGINT', \
                 unrestorable: 'BOOLEAN', forward_only: 'BOOLEAN', dev_version: 'VARCHAR', \
                 fingerprint: 'VARCHAR'}",
            ),
            (
                "_intervals",
                "{id: 'VARCHAR', created_ts: 'BIGINT', name: 'VARCHAR', identifier: 'VARCHAR', \
                 version: 'VARCHAR', dev_version: 'VARCHAR', start_ts: 'BIGINT', end_ts: 'BIGINT', \
                 is_dev: 'BOOLEAN', is_removed: 'BOOLEAN', is_compacted: 'BOOLEAN', \
                 is_pending_restatement: 'BOOLEAN', last_altered_ts: 'BIGINT'}",
            ),
        ];
        for (table, columns) in typed {
            conn.execute_batch(&format!(
                "CREATE TABLE sqlmesh.{table} AS SELECT * FROM read_json('{base}{table}.jsonl', \
                 format = 'newline_delimited', columns = {columns});"
            ))
            .unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_pinned() {
        let conn = fixture::connection();
        let v = read_versions(&conn, "memory", "sqlmesh").unwrap();
        assert_eq!(v.schema_version, 100);
        check_schema_version(&v).unwrap();
        let bad = Versions {
            schema_version: 104,
            ..v
        };
        let err = check_schema_version(&bad).unwrap_err().to_string();
        assert!(err.contains("104") && err.contains("100"), "{err}");
    }

    #[test]
    fn prod_resolves_to_the_prod_snapshot_not_the_dev_one() {
        let conn = fixture::connection();
        let env = read_environment(&conn, "memory", "sqlmesh", "prod").unwrap();
        assert!(env.finalized_ts.is_some());
        assert_eq!(env.snapshots.len(), 6);
        let models = read_models(&conn, "memory", "sqlmesh", &env).unwrap();
        assert_eq!(models.len(), 6);
        // `documented_model` has two `_snapshots` rows (prod + the dev edit)
        // under one name; the join must land on prod's.
        let doc = models
            .iter()
            .find(|m| m.name.table == "documented_model")
            .unwrap();
        assert_eq!(
            doc.description.as_deref(),
            Some("Orders per item with explicit columns and descriptions")
        );
        assert_eq!(doc.kind, "FULL");
        assert_eq!(doc.owner.as_deref(), Some("finance"));
        assert_eq!(doc.tags, vec!["invoicing", "gold"]);
        assert_eq!(doc.grain, vec!["item_id"]);
        assert_eq!(doc.declared_columns["num_orders"], "BIGINT");
        assert_eq!(doc.column_descriptions["item_id"], "The item identifier");
        assert_eq!(doc.depends_on, vec!["db.sqlmesh_example.incremental_model"]);

        let dev = read_environment(&conn, "memory", "sqlmesh", "dev_fixture").unwrap();
        let dev_models = read_models(&conn, "memory", "sqlmesh", &dev).unwrap();
        let dev_doc = dev_models
            .iter()
            .find(|m| m.name.table == "documented_model")
            .unwrap();
        assert_eq!(dev_doc.description.as_deref(), Some("CHANGED IN DEV"));
        assert_ne!(dev_doc.identifier, doc.identifier);
        assert_eq!(
            dev_doc.version, doc.version,
            "a metadata-only change keeps the data version"
        );
    }

    #[test]
    fn inline_comments_and_grains_come_through_the_state_read() {
        let conn = fixture::connection();
        let env = read_environment(&conn, "memory", "sqlmesh", "prod").unwrap();
        let models = read_models(&conn, "memory", "sqlmesh", &env).unwrap();
        let inline = models
            .iter()
            .find(|m| m.name.table == "inline_comment_model")
            .unwrap();
        assert!(inline.declared_columns.is_empty());
        assert_eq!(inline.column_descriptions["num_orders"], "number of orders");
        let inc = models
            .iter()
            .find(|m| m.name.table == "incremental_model")
            .unwrap();
        assert_eq!(inc.grain, vec!["id", "event_date"]);
        assert_eq!(inc.kind, "INCREMENTAL_BY_TIME_RANGE");
    }

    #[test]
    fn intervals_roll_up_per_model() {
        let conn = fixture::connection();
        let env = read_environment(&conn, "memory", "sqlmesh", "prod").unwrap();
        let models = read_models(&conn, "memory", "sqlmesh", &env).unwrap();
        let intervals = read_intervals(&conn, "memory", "sqlmesh", &models).unwrap();
        let inc = models
            .iter()
            .find(|m| m.name.table == "incremental_model")
            .unwrap();
        let row = &intervals[&inc.raw_name];
        assert!(row.start_ts < row.end_ts);
        assert!(!row.pending_restatement);
    }

    #[test]
    fn a_missing_environment_names_the_ones_present() {
        let conn = fixture::connection();
        let err = read_environment(&conn, "memory", "sqlmesh", "staging")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no environment named 'staging'"), "{err}");
        assert!(err.contains("dev_fixture") && err.contains("prod"), "{err}");
    }
}
