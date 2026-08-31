//! Discovered interfaces (ADR 0016): a modeling tool's **deployed** models,
//! read from the tool's own state, become a cell's exports — bound, typed,
//! described, with provenance — through one tool-agnostic IR (`ir`) and one
//! adapter per tool (`sqlmesh`). `datamk sync` is the build-side verb that
//! reads the tool (with credentials) and writes the sidecar record
//! (`record`) every credential-light consumer reads.

pub mod ir;
pub mod record;
pub mod select;
pub mod sqlmesh;
pub mod warehouse;

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use std::path::Path;

use crate::config::{
    Bindings, Contract, Discover, DiscoverFrom, OnUnresolvable, ResolvedConnection,
};
use ir::{ColumnsSource, DeployedCatalog, DeployedColumn, DeployedModel, Evidence, Interval};
use record::DeployedCatalogRecord;

/// Exit code `sync` uses for "not an error in the cell — the tool's
/// environment is mid-apply; try again" (ADR 0016 §3), so a scheduler can
/// treat it as retryable without parsing text.
pub const EXIT_RETRY: i32 = 75;

/// The error `sync` returns for a transient state (an unfinalized
/// environment); `main` maps it to `EXIT_RETRY`.
#[derive(Debug)]
pub struct RetryLater(pub String);

impl std::fmt::Display for RetryLater {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RetryLater {}

const STATE_ALIAS: &str = "__datamk_state";

/// A named profile connection, with a relative DuckDB path rebased against
/// the cell directory (the same rebase `config::load` does for sources).
fn named_connection(b: &Bindings, name: &str, dir: &Path) -> Result<ResolvedConnection> {
    let mut c = crate::config::resolve_named_connection(b, name)?;
    if let ResolvedConnection::Duckdb { path } = &mut c {
        if !Path::new(path.as_str()).is_absolute() {
            *path = dir.join(path.as_str()).to_string_lossy().into_owned();
        }
    }
    Ok(c)
}

/// Attach the state store read-only under `STATE_ALIAS` and return the
/// schema its tables live in.
fn attach_state(
    conn: &duckdb::Connection,
    name: &str,
    c: &ResolvedConnection,
    schema: &str,
) -> Result<String> {
    match c {
        ResolvedConnection::Postgres { .. } | ResolvedConnection::Duckdb { .. } => {
            let install = c.install_load_sql();
            if !install.is_empty() {
                conn.execute_batch(install).with_context(|| {
                    format!(
                        "loading the {} extension for connection '{name}'",
                        c.type_name()
                    )
                })?;
            }
            conn.execute_batch(&c.attach_sql(STATE_ALIAS))
                .map_err(|e| c.rewrite_attach_error(e, name))?;
            Ok(schema.to_string())
        }
        ResolvedConnection::Bigquery { .. } | ResolvedConnection::Snowflake { .. } => bail!(
            "connection '{name}' is {}: reading a SQLMesh state store held in {} is not \
             supported yet — SQLMesh's default (a duckdb file) and Postgres are. Point \
             `discover.state` at one of those.",
            c.type_name(),
            c.type_name()
        ),
    }
}

/// `datamk debug sqlmesh-comments <file>`: the differential check for the
/// inline-comment extractor against SQLMesh's own output, run wherever the
/// models are (their SQL never has to leave that machine).
pub fn debug_sqlmesh_comments(file: &Path) -> Result<()> {
    let raw =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let cases: IndexMap<String, serde_json::Value> =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", file.display()))?;
    let mut checked = 0usize;
    let mut mismatches = 0usize;
    for (model, case) in &cases {
        let Some(sql) = case.get("sql").and_then(|v| v.as_str()) else {
            continue;
        };
        let dialect = case.get("dialect").and_then(|v| v.as_str()).unwrap_or("");
        let expected: IndexMap<String, String> = case
            .get("expected")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let got = sqlmesh::comments::column_descriptions(sql, dialect);
        checked += 1;
        if got != expected {
            mismatches += 1;
            println!("MISMATCH {model} (dialect {dialect})");
            for (k, v) in &expected {
                match got.get(k) {
                    Some(g) if g == v => {}
                    Some(g) => println!("  {k}: expected {v:?}, got {g:?}"),
                    None => println!("  {k}: expected {v:?}, got nothing"),
                }
            }
            for (k, g) in &got {
                if !expected.contains_key(k) {
                    println!("  {k}: expected nothing, got {g:?}");
                }
            }
        }
    }
    println!("{checked} models checked, {mismatches} mismatches");
    if mismatches > 0 {
        bail!("{mismatches} of {checked} models disagree with SQLMesh's column_descriptions");
    }
    Ok(())
}

/// `datamk sync`: read the tool's deployed environment and the warehouse,
/// write `.cell/deployed_catalog.json`, and say what was found.
pub fn sync(file: &Path, profile: &str, dry_run: bool) -> Result<()> {
    // Pure parse first — a typo fails before any connection is opened.
    let def = crate::config::CellDef::load(file)?;
    let dir = crate::config::cell_dir(file);
    let Some(d) = def.discover.clone() else {
        bail!(
            "cell '{}' has no `discover:` block — `datamk sync` discovers an interface from a \
             modeling tool's deployed state; a hand-authored cell is built with `datamk run`.",
            def.cell
        );
    };
    let profile_path = dir.join("profiles").join(format!("{profile}.yaml"));
    let raw = Bindings::load(&profile_path)?;
    let state = named_connection(&raw, &d.state, &dir)
        .with_context(|| format!("resolving `discover.state: {}`", d.state))?;
    let wh = named_connection(&raw, &d.warehouse, &dir)
        .with_context(|| format!("resolving `discover.warehouse: {}`", d.warehouse))?;
    let cell_yaml_digest = crate::context::cell_yaml_digest_of(file)?;
    let now = crate::timeutil::unix_now();

    let conn = duckdb::Connection::open_in_memory().context("opening an in-memory DuckDB")?;
    let catalog = match d.from {
        DiscoverFrom::Sqlmesh => {
            let schema = attach_state(&conn, &d.state, &state, &d.state_schema)?;
            read_sqlmesh(&conn, &schema, &d, &wh, now)?
        }
    };

    // ADR 0016 §6: the authored version of every `supported` model, as it
    // stands now — stamped on the record so the next sync can tell "the
    // upstream data definition moved under an unchanged supported version"
    // and refuse to overwrite silently; the operator bumps the version or
    // excludes the model.
    let pins: IndexMap<String, String> = d
        .overrides
        .iter()
        .filter(|o| o.contract == Some(Contract::Supported))
        .map(|o| {
            (
                o.model.clone(),
                o.version.clone().unwrap_or_else(|| "1.0.0".to_string()),
            )
        })
        .collect();
    if let Ok(previous) = DeployedCatalogRecord::load(&dir) {
        for (object, version) in &pins {
            let (Some(old_version), Some(old), Some(new)) = (
                previous.pins.get(object),
                previous.catalog.models.iter().find(|m| &m.object == object),
                catalog.models.iter().find(|m| &m.object == object),
            ) else {
                continue;
            };
            if old.data_hash != new.data_hash && old_version == version {
                bail!(
                    "model '{object}' is `contract: supported` at version {version} and its \
                     data definition changed upstream (data_hash {} -> {}) since the last sync. \
                     A supported contract must not move silently: bump \
                     `discover.overrides[].version` for it (MAJOR if the meaning changed), or \
                     exclude the model.",
                    old.data_hash.as_deref().unwrap_or("?"),
                    new.data_hash.as_deref().unwrap_or("?")
                );
            }
        }
    }

    // A page an override names is validated here — at the step that has
    // credentials — not first at `context`/`serve`: materialize into a
    // scratch copy exactly as `config::load` will, and apply ADR 0013's
    // path and size rules to it.
    {
        let mut preview = def.clone();
        select::materialize(&mut preview, &d, &catalog, false)?;
        crate::config::docs::validate_all(&dir, &preview)
            .with_context(|| format!("validating cell definition {}", file.display()))?;
    }

    let record = DeployedCatalogRecord {
        written_at: crate::timeutil::rfc3339_utc(now),
        datamk_version: env!("CARGO_PKG_VERSION").to_string(),
        cell_yaml_digest,
        profile: profile.to_string(),
        pins,
        catalog,
    };
    summarize(&record, &d, dry_run);
    if !dry_run {
        let path = record.write(&dir)?;
        eprintln!("Wrote {}", path.display());
        eprintln!("Next:");
        eprintln!("  datamk verify  -p {profile}    # live-check types against the warehouse");
        eprintln!("  datamk context -p {profile}    # the document agents read");
        eprintln!("  datamk serve   -p {profile}    # /context + /openapi.json; rows stay in the warehouse");
    }
    Ok(())
}

fn summarize(record: &DeployedCatalogRecord, d: &Discover, dry_run: bool) {
    let c = &record.catalog;
    let selected = c.models.len();
    // Column descriptions by origin: the tool's, the warehouse's, none.
    let mut by_origin = [0usize; 3];
    for m in &c.models {
        for col in m.columns.values() {
            match (col.description.as_ref(), col.description_source) {
                (None, _) => by_origin[2] += 1,
                (Some(_), Some(ColumnsSource::Warehouse)) => by_origin[1] += 1,
                (Some(_), _) => by_origin[0] += 1,
            }
        }
    }
    eprintln!(
        "{}Discovered {} models from {} environment '{}' (plan {}…, finalized {}); {} selected",
        if dry_run { "[dry run] " } else { "" },
        c.total_models,
        c.tool,
        c.environment,
        &c.plan_id[..c.plan_id.len().min(8)],
        c.finalized_at,
        selected
    );
    eprintln!(
        "  {selected} exports · {} excluded by discover.select · column descriptions: {} from {}, {} from the warehouse, {} none",
        c.unselected_models, by_origin[0], c.tool, by_origin[1], by_origin[2]
    );
    if by_origin[0] + by_origin[1] == 0 && by_origin[2] > 0 {
        eprintln!(
            "Warning: 0 of {} columns carry a description. SQLMesh writes model and column \
             comments onto warehouse objects only when `register_comments` is enabled (on by \
             default); until it is, and the models are re-planned, descriptions will be absent.",
            by_origin[2]
        );
    }
    let _ = d;
}

/// The SQLMesh adapter's whole read: state tables, selection, warehouse
/// columns, resolution order (§4), IR.
fn read_sqlmesh(
    conn: &duckdb::Connection,
    schema: &str,
    d: &Discover,
    wh: &ResolvedConnection,
    now: i64,
) -> Result<DeployedCatalog> {
    use sqlmesh::state;
    let versions = state::read_versions(conn, STATE_ALIAS, schema)?;
    state::check_schema_version(&versions)?;
    let env = state::read_environment(conn, STATE_ALIAS, schema, &d.environment)?;
    let Some(finalized_ts) = env.finalized_ts else {
        return Err(RetryLater(format!(
            "environment '{}' is not finalized (plan {} is in flight — SQLMesh stamps \
             finalized_ts only after the virtual layer swap completes). Retry once the apply \
             finishes; datamk exits {EXIT_RETRY} for this case so a scheduler can retry.",
            env.name, env.plan_id
        ))
        .into());
    };
    let all = state::read_models(conn, STATE_ALIAS, schema, &env)?;
    let total = all.len();
    let (selected, unselected): (Vec<state::StateModel>, Vec<state::StateModel>) = all
        .into_iter()
        .partition(|m| select::selected(d, &m.kind, &m.name.schema, &m.name.object(), &m.tags));
    let unselected_objects: std::collections::HashSet<String> =
        unselected.iter().map(|m| m.name.object()).collect();
    if selected.is_empty() {
        bail!(
            "`discover.select` matched none of the {total} models in environment '{}' — \
             check the tags/schemas/models it names (schemas and models are unquoted, without \
             the catalog: `invoice.flight_spend`).",
            env.name
        );
    }
    // An override names a model; if that model isn't among the selected
    // ones the promise it carries (a version, a contract, a docs page)
    // would vanish from the document without a word — say so, or refuse.
    let selected_objects: std::collections::HashSet<String> =
        selected.iter().map(|m| m.name.object()).collect();
    for o in &d.overrides {
        if selected_objects.contains(&o.model) {
            continue;
        }
        let why = if unselected_objects.contains(&o.model) {
            "is deployed but excluded by `discover.select`/`exclude`"
        } else {
            "is no longer deployed in this environment (renamed, moved to another schema, or removed)"
        };
        let msg = format!(
            "override for model '{}' {why} — the export{}{} it promised will not appear",
            o.model,
            o.as_name
                .as_deref()
                .map(|a| format!(" '{a}'"))
                .unwrap_or_default(),
            o.docs
                .as_deref()
                .map(|p| format!(" and its docs page {p}"))
                .unwrap_or_default()
        );
        match d.on_missing_override {
            crate::config::OnMissingOverride::Warn => tracing::warn!("{msg}"),
            crate::config::OnMissingOverride::Fail => bail!(
                "{msg}. Update or remove the override, or set `discover.on_missing_override: warn`."
            ),
        }
    }
    let intervals = state::read_intervals(conn, STATE_ALIAS, schema, &selected)?;

    // Warehouse read, batched per schema, for every selected model — the
    // primary source of column definitions (§4).
    let objects: Vec<warehouse::ObjectRef> = selected
        .iter()
        .map(|m| warehouse::ObjectRef {
            catalog: m.name.catalog.clone(),
            schema: m.name.schema.clone(),
            table: m.name.table.clone(),
        })
        .collect();
    let wh_columns = warehouse::read_columns(
        conn,
        &warehouse::Warehouse {
            name: d.warehouse.clone(),
            resolved: wh.clone(),
        },
        &objects,
    )?;

    let mut models = Vec::with_capacity(selected.len());
    let mut excluded_unresolvable = Vec::new();
    for m in selected {
        let object = m.name.object();
        let wh = wh_columns.get(&object);
        let (columns, columns_source): (IndexMap<String, DeployedColumn>, ColumnsSource) =
            if !m.declared_columns.is_empty() {
                (
                    m.declared_columns
                        .iter()
                        .map(|(col, ty)| {
                            (
                                col.clone(),
                                DeployedColumn {
                                    ty: ty.clone(),
                                    description: None,
                                    description_source: None,
                                },
                            )
                        })
                        .collect(),
                    ColumnsSource::Declared,
                )
            } else if let Some(wh) = wh.filter(|w| !w.columns.is_empty()) {
                (
                    wh.columns
                        .iter()
                        .map(|(col, ty)| {
                            (
                                col.clone(),
                                DeployedColumn {
                                    ty: ty.clone(),
                                    description: None,
                                    description_source: None,
                                },
                            )
                        })
                        .collect(),
                    ColumnsSource::Warehouse,
                )
            } else {
                match d.on_unresolvable {
                    OnUnresolvable::Fail => bail!(
                        "model {} has no resolvable column types: its MODEL(...) declares no \
                         `columns`, and the warehouse (connection '{}') has no object \
                         `{object}`. Fix one of: declare `columns` in the model; run `sqlmesh plan \
                         {}` so the object exists; grant '{}' metadata read on schema '{}'; or \
                         set `discover.on_unresolvable: exclude`.",
                        m.name.unquoted(),
                        d.warehouse,
                        env.name,
                        d.warehouse,
                        m.name.schema
                    ),
                    OnUnresolvable::Exclude => {
                        excluded_unresolvable.push(m.name.unquoted());
                        continue;
                    }
                }
            };
        let mut columns = columns;
        for (col, c) in columns.iter_mut() {
            if let Some(text) = m
                .column_descriptions
                .get(col)
                .filter(|t| !t.trim().is_empty())
            {
                c.description = Some(text.clone());
                c.description_source = Some(ColumnsSource::Declared);
            } else if let Some(text) = wh.and_then(|w| w.descriptions.get(col)) {
                c.description = Some(text.clone());
                c.description_source = Some(ColumnsSource::Warehouse);
            }
        }
        let description = m
            .description
            .clone()
            .or_else(|| wh.and_then(|w| w.table_description.clone()));
        let interval = intervals.get(&m.raw_name).map(|r| Interval {
            start: crate::timeutil::rfc3339_utc(r.start_ts / 1000),
            end: crate::timeutil::rfc3339_utc(r.end_ts / 1000),
        });
        models.push(DeployedModel {
            name: m.name.unquoted(),
            object,
            catalog: m.name.catalog.clone(),
            fingerprint: m.identifier.clone(),
            version: Some(m.version.clone()),
            data_hash: Some(m.data_hash.clone()),
            kind: m.kind.clone(),
            cron: m.cron.clone(),
            owner: m.owner.clone(),
            tags: m.tags.clone(),
            description,
            columns,
            columns_source,
            grain: m.grain.clone(),
            depends_on: m.depends_on.clone(),
            intervals: interval,
            pending_restatement: intervals.get(&m.raw_name).map(|r| r.pending_restatement),
        });
    }
    if !excluded_unresolvable.is_empty() {
        tracing::warn!(
            models = ?excluded_unresolvable,
            "excluded by discover.on_unresolvable: exclude — no resolvable column types"
        );
    }
    let unselected = total - models.len() - excluded_unresolvable.len();
    Ok(DeployedCatalog {
        tool: "sqlmesh".to_string(),
        environment: env.name.clone(),
        plan_id: env.plan_id.clone(),
        finalized_at: crate::timeutil::rfc3339_utc(finalized_ts / 1000),
        synced_at: crate::timeutil::rfc3339_utc(now),
        evidence: Evidence::EnvironmentRow,
        schema_version: versions.schema_version.to_string(),
        models,
        total_models: total,
        unselected_models: unselected + excluded_unresolvable.len(),
    })
}

#[cfg(test)]
mod tests {
    //! End to end on disk (ADR 0016): a discovered cell over a real DuckDB
    //! state + warehouse file built from the checked-in fixtures — `sync`
    //! writes the record, `config::load` materializes the interface,
    //! `context` describes it with provenance, `verify` live-checks it, and
    //! the staleness/refusal paths say what they should.
    use super::*;
    use std::path::PathBuf;

    fn scaffold(tag: &str, cell_yaml: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datamk-catalog-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(
            dir.join("docs/documented.md"),
            "# Documented\n\nHow to use this export, at length.",
        )
        .unwrap();
        sqlmesh::state::fixture::build_file(&dir.join("state.db"));
        std::fs::write(dir.join("cell.yaml"), cell_yaml).unwrap();
        std::fs::write(
            dir.join("profiles/local.yaml"),
            "catalog: ./.cell/catalog.ducklake\n\
             storage: ./.cell/data\n\
             channels: [\"Rows live in the SQLMesh duckdb file; attach it read-only.\"]\n\
             connections:\n\
             \x20 state:\n\
             \x20   type: duckdb\n\
             \x20   path: ./state.db\n\
             \x20 wh:\n\
             \x20   type: duckdb\n\
             \x20   path: ./state.db\n",
        )
        .unwrap();
        dir
    }

    const CELL: &str = "cell: example\n\
        description: The fixture project's gold models.\n\
        discover:\n\
        \x20 from: sqlmesh\n\
        \x20 state: state\n\
        \x20 warehouse: wh\n\
        \x20 select:\n\
        \x20   schemas: [sqlmesh_example]\n\
        \x20 overrides:\n\
        \x20   - model: sqlmesh_example.documented_model\n\
        \x20     as: documented\n\
        \x20     version: 2.0.0\n\
        \x20     contract: supported\n\
        \x20     description: Authored, so it wins.\n\
        \x20     docs: docs/documented.md\n";

    /// ADR 0017 amendment: a stale sync record defers `applies_to`
    /// validation forever (it runs only when the interface materializes), so
    /// the portable document must say the claims are unvalidated — while
    /// still listing the authored definitions themselves.
    #[test]
    fn stale_discovery_emits_definitions_with_an_unvalidated_note() {
        let cell = "cell: example\n\
            description: The fixture project's gold models.\n\
            definitions:\n\
            \x20 - term: net_revenue\n\
            \x20   description: Invoiced revenue less credit memos.\n\
            \x20   applies_to: [documented@2.id]\n\
            \x20 - term: fiscal_year\n\
            \x20   description: Starts Feb 1.\n\
            discover:\n\
            \x20 from: sqlmesh\n\
            \x20 state: state\n\
            \x20 warehouse: wh\n\
            \x20 select:\n\
            \x20   schemas: [sqlmesh_example]\n";
        let dir = scaffold("stale-defs", cell);
        // No sync ran: the record is missing, discovery is stale.
        let doc =
            crate::context::build_document_for(&dir.join("cell.yaml"), "local", true, None, None)
                .expect("the portable door still emits on a stale record");
        assert!(doc.exports.is_empty());
        let terms: Vec<&str> = doc.definitions.iter().map(|d| d.term.as_str()).collect();
        assert_eq!(terms, vec!["net_revenue", "fiscal_year"]);
        assert!(
            doc.notes
                .iter()
                .any(|n| n.contains("No exports are listed")),
            "{:?}",
            doc.notes
        );
        assert!(
            doc.notes
                .iter()
                .any(|n| n.contains("applies_to") && n.contains("not been validated")),
            "{:?}",
            doc.notes
        );
    }

    #[test]
    fn sync_then_load_then_context_then_verify_end_to_end() {
        let dir = scaffold("e2e", CELL);
        let file = dir.join("cell.yaml");

        // Before sync: load succeeds, interface empty, discovery stale.
        let loaded = crate::config::load(&file, "local").unwrap();
        assert!(loaded.def.interface.is_empty());
        assert!(matches!(
            loaded.discovery,
            Some(crate::config::Discovery::Stale(record::Staleness::Missing))
        ));

        sync(&file, "local", false).expect("sync");
        let record = DeployedCatalogRecord::load(&dir).unwrap();
        assert_eq!(record.catalog.tool, "sqlmesh");
        assert_eq!(record.catalog.environment, "prod");
        assert_eq!(record.catalog.total_models, 6);
        // SEED excluded by default: 5 selected.
        assert_eq!(record.catalog.models.len(), 5);
        assert_eq!(record.catalog.unselected_models, 1);
        assert_eq!(
            record
                .pins
                .get("sqlmesh_example.documented_model")
                .map(String::as_str),
            Some("2.0.0")
        );

        let loaded = crate::config::load(&file, "local").unwrap();
        assert!(matches!(
            loaded.discovery,
            Some(crate::config::Discovery::Fresh { exports: 5, .. })
        ));
        assert_eq!(loaded.def.interface.len(), 5);
        assert_eq!(loaded.def.sources.len(), 5);

        let doc = crate::context::build_document(&file, "local", false).unwrap();
        let v = serde_json::to_value(&doc).unwrap();
        assert_eq!(v["discovered_from"]["tool"], "sqlmesh");
        // overrides[].docs: the page rides the export's route key, inlined.
        let page = v["docs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["target"] == "documented@2")
            .expect("an override's docs page");
        assert_eq!(page["source_path"], "docs/documented.md");
        assert!(page["content"]
            .as_str()
            .unwrap()
            .contains("How to use this export"));
        assert_eq!(v["discovered_from"]["environment"], "prod");
        assert_eq!(v["discovered_from"]["evidence"], "environment_row");
        assert!(v["discovered_from"]["plan_id"].is_string());
        assert_eq!(v["status"], "draft");
        let exports = v["exports"].as_array().unwrap();
        let by_name = |n: &str| {
            exports
                .iter()
                .find(|e| e["name"] == n)
                .unwrap_or_else(|| panic!("{n}"))
        };

        // The override: authored version/contract/description win and say so.
        let doc_m = by_name("documented");
        assert_eq!(doc_m["route"], "documented@2");
        assert_eq!(doc_m["contract"], "supported");
        assert_eq!(doc_m["description"], "Authored, so it wins.");
        assert_eq!(doc_m["from"]["description"], "cell.yaml");
        assert_eq!(doc_m["from"]["grain"], "sqlmesh");
        assert_eq!(doc_m["grain"], serde_json::json!(["item_id"]));
        // Declared columns/descriptions come from the model definition.
        assert_eq!(doc_m["schema"]["item_id"]["type"], "INT");
        assert_eq!(doc_m["schema"]["item_id"]["from"]["type"], "sqlmesh");
        assert_eq!(
            doc_m["schema"]["item_id"]["description"],
            "The item identifier"
        );
        assert_eq!(doc_m["schema"]["item_id"]["from"]["description"], "sqlmesh");
        assert!(doc_m["query"].is_null(), "bound: no data route");
        assert_eq!(
            doc_m["binding"]["object"],
            "sqlmesh_example.documented_model"
        );
        assert_eq!(doc_m["binding"]["connection"], "wh");
        assert_eq!(doc_m["deployed"]["kind"], "FULL");
        assert_eq!(doc_m["deployed"]["owner"], "finance");
        assert_eq!(
            doc_m["deployed"]["tags"],
            serde_json::json!(["invoicing", "gold"])
        );
        assert!(
            doc_m["deployed"]["at"].is_string() && doc_m["deployed"]["fingerprint"].is_string()
        );
        assert!(doc_m["deployed"]["intervals"]["start"].is_string());
        assert_eq!(
            doc_m["depends_on"],
            serde_json::json!(["sqlmesh_example_incremental_model@1"])
        );

        // Inline comments: SQLMesh's, via the state store — and types from
        // the warehouse since the model declares none.
        let inline = by_name("sqlmesh_example_inline_comment_model");
        assert_eq!(
            inline["description"],
            "View with inline column comments only"
        );
        assert_eq!(inline["from"]["description"], "sqlmesh");
        assert_eq!(inline["schema"]["num_orders"]["type"], "BIGINT");
        assert_eq!(inline["schema"]["num_orders"]["from"]["type"], "warehouse");
        assert_eq!(
            inline["schema"]["num_orders"]["description"],
            "number of orders"
        );
        assert_eq!(
            inline["schema"]["num_orders"]["from"]["description"],
            "sqlmesh"
        );
        assert_eq!(inline["deployed"]["kind"], "VIEW");

        // A model with nothing declared anywhere: types from the warehouse,
        // no descriptions, no `from.description`.
        let full = by_name("sqlmesh_example_full_model");
        assert_eq!(full["schema"]["item_id"]["from"]["type"], "warehouse");
        assert!(full["schema"]["item_id"].get("description").is_none());
        // Lineage counts an unselected (SEED) parent instead of naming it.
        let inc = by_name("sqlmesh_example_incremental_model");
        assert_eq!(inc["depends_on_unselected"], 1);
        assert!(inc.get("depends_on").is_none());
        assert_eq!(inc["grain"], serde_json::json!(["id", "event_date"]));

        // The dev environment's edit never leaks into prod's document.
        assert!(!serde_json::to_string(&v)
            .unwrap()
            .contains("CHANGED IN DEV"));

        // `verify` live-checks the bound exports against the duckdb file.
        crate::verify::run(&file, "local").expect("verify the discovered cell");
        let doc = crate::context::build_document(&file, "local", true).unwrap();
        let v = serde_json::to_value(&doc).unwrap();
        assert_eq!(v["status"], "verified_at_source", "{v}");

        // `run` refuses with the sync hint; `attach`/`rollback` refuse.
        let err = crate::engine::run(&file, "local", None, crate::engine::RunOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("datamk sync"), "{err}");
        let err = crate::ops::attach(&file, "local", None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("discovers its interface"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_supported_models_upstream_change_refuses_to_sync_until_the_version_moves() {
        let dir = scaffold("pin", CELL);
        let file = dir.join("cell.yaml");
        sync(&file, "local", false).unwrap();
        // Simulate an upstream data change: rewrite the record's data_hash
        // for the supported model, as a previous sync would have seen it.
        let mut record = DeployedCatalogRecord::load(&dir).unwrap();
        let m = record
            .catalog
            .models
            .iter_mut()
            .find(|m| m.object == "sqlmesh_example.documented_model")
            .unwrap();
        m.data_hash = Some("previous".to_string());
        record.write(&dir).unwrap();
        let err = sync(&file, "local", false).unwrap_err().to_string();
        assert!(
            err.contains("contract: supported") && err.contains("2.0.0"),
            "{err}"
        );
        // Bumping the version is the operator's answer.
        let bumped = CELL.replace("version: 2.0.0", "version: 3.0.0");
        std::fs::write(&file, bumped).unwrap();
        sync(&file, "local", false).expect("a bumped version syncs");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_records_are_named_not_served() {
        let dir = scaffold("stale", CELL);
        let file = dir.join("cell.yaml");
        sync(&file, "local", false).unwrap();
        // A profile switch: the record attests `local`, not `prod`.
        std::fs::copy(
            dir.join("profiles/local.yaml"),
            dir.join("profiles/prod.yaml"),
        )
        .unwrap();
        let loaded = crate::config::load(&file, "prod").unwrap();
        match loaded.discovery {
            Some(crate::config::Discovery::Stale(record::Staleness::Profile { .. })) => {}
            other => panic!("{other:?}"),
        }
        assert!(loaded.def.interface.is_empty());
        let doc = crate::context::build_document(&file, "prod", true).unwrap();
        assert!(
            doc.notes
                .iter()
                .any(|n| n.contains("synced under profile 'local'")),
            "{:?}",
            doc.notes
        );
        assert!(doc.exports.is_empty());

        // A cell.yaml edit invalidates it too.
        std::fs::write(&file, format!("{CELL}access:\n  shareable: true\n")).unwrap();
        let loaded = crate::config::load(&file, "local").unwrap();
        assert!(matches!(
            loaded.discovery,
            Some(crate::config::Discovery::Stale(
                record::Staleness::CellYamlChanged
            ))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_docs_page_fails_at_sync_not_at_serve() {
        let dir = scaffold("docs", CELL);
        let file = dir.join("cell.yaml");
        let absolute = dir.join("docs/documented.md").display().to_string();
        std::fs::write(
            &file,
            CELL.replace("docs: docs/documented.md", &format!("docs: {absolute}")),
        )
        .unwrap();
        let err = format!("{:#}", sync(&file, "local", false).unwrap_err());
        assert!(
            err.contains("absolute paths and `..` are rejected"),
            "{err}"
        );
        // A page that exists but sits OUTSIDE the cell directory — the file
        // must really be there, or the read fails before the escape check.
        let outside = dir.parent().unwrap().join(format!(
            "outside-{}.md",
            dir.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&outside, "x").unwrap();
        let rel = format!("../{}", outside.file_name().unwrap().to_string_lossy());
        std::fs::write(
            &file,
            CELL.replace("docs: docs/documented.md", &format!("docs: {rel}")),
        )
        .unwrap();
        let err = format!("{:#}", sync(&file, "local", false).unwrap_err());
        assert!(
            err.contains("absolute paths and `..` are rejected"),
            "{err}"
        );
        let _ = std::fs::remove_file(&outside);
        std::fs::write(dir.join("docs/documented.md"), "x".repeat(70_000)).unwrap();
        std::fs::write(&file, CELL).unwrap();
        let err = format!("{:#}", sync(&file, "local", false).unwrap_err());
        assert!(err.contains("max 65536"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An override whose model is gone or unselected warns by default and
    /// can be made to fail — never vanishes silently.
    #[test]
    fn a_missing_override_warns_by_default_and_fails_on_request() {
        let dir = scaffold(
            "missing",
            &CELL.replace(
                "    - model: sqlmesh_example.documented_model\n",
                "    - model: sqlmesh_example.renamed_away\n      as: gone\n    - model: sqlmesh_example.documented_model\n",
            ),
        );
        let file = dir.join("cell.yaml");
        sync(&file, "local", false).expect("warn is the default");
        let doc = crate::context::build_document(&file, "local", true).unwrap();
        assert!(doc.exports.iter().all(|e| e.name != "gone"));
        let strict = std::fs::read_to_string(&file).unwrap().replace(
            "  overrides:\n",
            "  on_missing_override: fail\n  overrides:\n",
        );
        std::fs::write(&file, strict).unwrap();
        let err = format!("{:#}", sync(&file, "local", false).unwrap_err());
        assert!(
            err.contains("sqlmesh_example.renamed_away") && err.contains("no longer deployed"),
            "{err}"
        );
        // Deployed but excluded by select says so too.
        let excluded = std::fs::read_to_string(&file)
            .unwrap()
            .replace("sqlmesh_example.renamed_away", "sqlmesh_example.seed_model");
        std::fs::write(&file, excluded).unwrap();
        let err = format!("{:#}", sync(&file, "local", false).unwrap_err());
        assert!(err.contains("excluded by `discover.select`"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn selection_that_matches_nothing_and_a_missing_environment_fail_loud() {
        let dir = scaffold(
            "select",
            &CELL.replace("schemas: [sqlmesh_example]", "tags: [nope]"),
        );
        let file = dir.join("cell.yaml");
        let err = sync(&file, "local", false).unwrap_err().to_string();
        assert!(err.contains("matched none of the 6 models"), "{err}");
        std::fs::write(
            &file,
            CELL.replace("state: state\n", "state: state\n  environment: staging\n"),
        )
        .unwrap();
        let err = sync(&file, "local", false).unwrap_err().to_string();
        assert!(err.contains("no environment named 'staging'"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_is_exclusive_with_authored_sections_and_needs_a_selection() {
        let dir = scaffold(
            "exclusive",
            &format!("{CELL}interface:\n  - name: x\n    version: 1.0.0\n"),
        );
        let err = format!(
            "{:#}",
            crate::config::CellDef::load(&dir.join("cell.yaml")).unwrap_err()
        );
        assert!(err.contains("cannot also declare `interface:`"), "{err}");
        std::fs::write(
            dir.join("cell.yaml"),
            "cell: c\ndiscover:\n  from: sqlmesh\n  state: s\n  warehouse: w\n",
        )
        .unwrap();
        let err = format!(
            "{:#}",
            crate::config::CellDef::load(&dir.join("cell.yaml")).unwrap_err()
        );
        assert!(err.contains("must name at least one of"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
