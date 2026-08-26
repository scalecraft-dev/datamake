//! Selection and materialization (ADR 0016 §1, §6): which deployed models
//! a `discover:` block admits, what each is called, and how a selected
//! `DeployedModel` becomes the bound `Export` + synthesized `Source` every
//! consumer of `def.interface` reads. Pure — no I/O — so `sync` (deciding
//! what to write) and `config::load` (reading it back) apply the exact same
//! rules.

use anyhow::{bail, Result};
use indexmap::IndexMap;
use std::collections::HashSet;

use crate::config::{
    is_valid_identifier, CellDef, ColumnSpec, Contract, Discover, DiscoveredExport, DiscoveredFrom,
    Export, FromMap, Origin, Override, Source, Visibility,
};

use super::ir::{ColumnsSource, DeployedCatalog, DeployedModel};

/// Kinds left out unless `select.kinds` names them: an `external_models.yaml`
/// entry is an upstream, not a product; a seed is a fixture.
pub const DEFAULT_EXCLUDED_KINDS: &[&str] = &["EXTERNAL", "SEED"];

/// Whether `d.select`/`d.exclude` admit a model with these facts.
pub fn selected(d: &Discover, kind: &str, schema: &str, object: &str, tags: &[String]) -> bool {
    let kind_ok = if d.select.kinds.is_empty() {
        !DEFAULT_EXCLUDED_KINDS
            .iter()
            .any(|k| k.eq_ignore_ascii_case(kind))
    } else {
        d.select.kinds.iter().any(|k| k.eq_ignore_ascii_case(kind))
    };
    if !kind_ok {
        return false;
    }
    // OR within a key, AND across the keys that are set: `schemas: [a]` +
    // `tags: [t]` is "models in schema a that carry tag t" — the guide's
    // rule, and the one that doesn't silently over-select into another
    // catalog.
    let by_tag = d.select.tags.is_empty() || d.select.tags.iter().any(|t| tags.contains(t));
    let by_schema = d.select.schemas.is_empty() || d.select.schemas.iter().any(|s| s == schema);
    let by_model = d.select.models.is_empty() || d.select.models.iter().any(|m| m == object);
    if !(by_tag && by_schema && by_model) {
        return false;
    }
    if d.exclude.models.iter().any(|m| m == object)
        || d.exclude.schemas.iter().any(|s| s == schema)
        || d.exclude.tags.iter().any(|t| tags.contains(t))
    {
        return false;
    }
    true
}

/// `<schema>_<table>`, lowercased, every non-identifier character folded
/// to `_` — a valid export name by construction (the catalog is dropped:
/// it is the environment, and the same `cell.yaml` must mean the same
/// thing against a staging state store).
pub fn mangled_name(object: &str) -> String {
    let mut out = String::with_capacity(object.len());
    for c in object.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

fn override_for<'a>(d: &'a Discover, object: &str) -> Option<&'a Override> {
    d.overrides.iter().find(|o| o.model == object)
}

/// The export name each model gets, with collisions after mangling a hard
/// error naming both models and the fix.
pub fn export_names(d: &Discover, models: &[DeployedModel]) -> Result<Vec<String>> {
    let mut names = Vec::with_capacity(models.len());
    let mut owners: IndexMap<String, &str> = IndexMap::new();
    for m in models {
        let name = match override_for(d, &m.object).and_then(|o| o.as_name.clone()) {
            Some(n) => n,
            None => mangled_name(&m.object),
        };
        if !is_valid_identifier(&name) {
            bail!(
                "model '{}' would be exported as '{name}', which is not a valid export name — \
                 set `discover.overrides[].as` for it",
                m.object
            );
        }
        if let Some(other) = owners.get(&name) {
            bail!(
                "models '{other}' and '{}' both map to export name '{name}' — give one of them \
                 `discover.overrides[].as: <other name>`",
                m.object
            );
        }
        owners.insert(name.clone(), &m.object);
        names.push(name);
    }
    Ok(names)
}

/// The document's `depends_on` for one model: route keys of selected
/// parents, plus how many parents selection left out.
fn lineage(model: &DeployedModel, by_model_name: &IndexMap<&str, String>) -> (Vec<String>, usize) {
    let mut routes = Vec::new();
    let mut unselected = 0;
    for parent in &model.depends_on {
        match by_model_name.get(parent.as_str()) {
            Some(route) => routes.push(route.clone()),
            None => unselected += 1,
        }
    }
    (routes, unselected)
}

/// Materialize the catalog into `def.interface` and `def.sources` (and
/// `def.discovered_from`). Every export is bound to a synthesized
/// `Source::Connection` on `discover.warehouse`, named like the export; the
/// tool's and the warehouse's claims carry their origins in `from`
/// (ADR 0015 §2); overrides win and are `cell.yaml`'s.
///
/// `keep_catalog`: the warehouse connection can address another catalog
/// (BigQuery: a project), so the synthesized source names the model's own
/// — `project.dataset.table` — and `verify` reads it from there. A
/// single-database connection (Postgres, a DuckDB file) gets
/// `schema.table`.
pub fn materialize(
    def: &mut CellDef,
    d: &Discover,
    catalog: &DeployedCatalog,
    keep_catalog: bool,
) -> Result<()> {
    let names = export_names(d, &catalog.models)?;
    // Route keys by model name, for lineage — computed before the exports
    // so a parent later in the list still resolves.
    let mut by_model_name: IndexMap<&str, String> = IndexMap::new();
    for (m, name) in catalog.models.iter().zip(&names) {
        let o = override_for(d, &m.object);
        let version = o
            .and_then(|o| o.version.clone())
            .unwrap_or_else(|| "1.0.0".to_string());
        let major = semver::Version::parse(&version)
            .map(|v| v.major)
            .unwrap_or(1);
        by_model_name.insert(m.name.as_str(), format!("{name}@{major}"));
    }

    let mut exports = Vec::with_capacity(catalog.models.len());
    let mut sources = IndexMap::new();
    let mut seen_sources: HashSet<String> = HashSet::new();
    for (m, name) in catalog.models.iter().zip(&names) {
        let o = override_for(d, &m.object);

        let mut schema: IndexMap<String, ColumnSpec> = IndexMap::new();
        for (col, c) in &m.columns {
            let mut from = FromMap::new();
            from.insert(
                "type".to_string(),
                match m.columns_source {
                    ColumnsSource::Declared => Origin::Sqlmesh,
                    ColumnsSource::Warehouse => Origin::Warehouse,
                },
            );
            if c.description.is_some() {
                from.insert(
                    "description".to_string(),
                    match c.description_source {
                        Some(ColumnsSource::Warehouse) => Origin::Warehouse,
                        _ => Origin::Sqlmesh,
                    },
                );
            }
            schema.insert(
                col.clone(),
                ColumnSpec {
                    ty: c.ty.clone(),
                    unit: None,
                    description: c.description.clone(),
                    from,
                },
            );
        }

        let mut from = FromMap::new();
        let description = match o.and_then(|o| o.description.clone()) {
            Some(text) => {
                from.insert("description".to_string(), Origin::CellYaml);
                Some(text)
            }
            None => {
                if m.description.is_some() {
                    from.insert("description".to_string(), Origin::Sqlmesh);
                }
                m.description.clone()
            }
        };
        let grain = match o.and_then(|o| o.grain.clone()) {
            Some(g) => {
                from.insert("grain".to_string(), Origin::CellYaml);
                g
            }
            None => {
                if !m.grain.is_empty() {
                    from.insert("grain".to_string(), Origin::Sqlmesh);
                }
                m.grain.clone()
            }
        };

        let (depends_on, depends_on_unselected) = lineage(m, &by_model_name);
        let source_name = name.clone();
        if !seen_sources.insert(source_name.clone()) {
            bail!("internal error: duplicate synthesized source '{source_name}'");
        }
        sources.insert(
            source_name.clone(),
            Source::Connection {
                connection: d.warehouse.clone(),
                table: Some(match (&m.catalog, keep_catalog) {
                    (Some(c), true) => format!("{c}.{}", m.object),
                    _ => m.object.clone(),
                }),
                query: None,
                incremental: None,
            },
        );
        exports.push(Export {
            name: name.clone(),
            version: o
                .and_then(|o| o.version.clone())
                .unwrap_or_else(|| "1.0.0".to_string()),
            source: None,
            bind: Some(source_name),
            description,
            docs: o.and_then(|o| o.docs.clone()),
            grain,
            schema,
            freshness: None,
            visibility: o
                .and_then(|o| o.visibility)
                .unwrap_or(Visibility::Discoverable),
            contract: o.and_then(|o| o.contract).unwrap_or(Contract::Experimental),
            from,
            discovered: Some(DiscoveredExport {
                model: m.name.clone(),
                kind: m.kind.clone(),
                cron: m.cron.clone(),
                owner: m.owner.clone(),
                tags: m.tags.clone(),
                fingerprint: m.fingerprint.clone(),
                version: m.version.clone(),
                data_hash: m.data_hash.clone(),
                intervals: m.intervals.clone(),
                pending_restatement: m.pending_restatement,
                depends_on,
                depends_on_unselected,
                at: catalog.synced_at.clone(),
            }),
        });
    }
    def.sources = sources;
    def.interface = exports;
    def.discovered_from = Some(DiscoveredFrom {
        tool: catalog.tool.clone(),
        environment: catalog.environment.clone(),
        plan_id: catalog.plan_id.clone(),
        finalized_at: catalog.finalized_at.clone(),
        synced_at: catalog.synced_at.clone(),
        evidence: catalog.evidence,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ir::{DeployedColumn, Evidence};

    fn discover(yaml: &str) -> Discover {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn model(object: &str, kind: &str, tags: &[&str]) -> DeployedModel {
        DeployedModel {
            name: format!("db.{object}"),
            object: object.to_string(),
            catalog: Some("db".to_string()),
            fingerprint: "1".to_string(),
            version: Some("1".to_string()),
            data_hash: Some("d".to_string()),
            kind: kind.to_string(),
            cron: None,
            owner: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            description: None,
            columns: IndexMap::new(),
            columns_source: ColumnsSource::Warehouse,
            grain: Vec::new(),
            depends_on: Vec::new(),
            intervals: None,
            pending_restatement: None,
        }
    }

    #[test]
    fn selection_is_or_within_and_across_with_defaults_excluding_external_and_seed() {
        let d = discover(
            "from: sqlmesh\nstate: s\nwarehouse: w\nselect:\n  tags: [gold]\n  schemas: [invoice]\nexclude:\n  models: [invoice.scratch]\n",
        );
        let t = |kind: &str, schema: &str, object: &str, tags: &[&str]| {
            let tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
            selected(&d, kind, schema, object, &tags)
        };
        // AND across keys: in `invoice` AND tagged `gold`.
        assert!(t("FULL", "invoice", "invoice.x", &["gold"]));
        assert!(
            !t("FULL", "marts", "marts.x", &["gold"]),
            "right tag, wrong schema"
        );
        assert!(
            !t("VIEW", "invoice", "invoice.y", &[]),
            "right schema, no tag"
        );
        assert!(!t("FULL", "marts", "marts.z", &["silver"]));
        assert!(
            !t("EXTERNAL", "invoice", "invoice.ext", &["gold"]),
            "EXTERNAL excluded by default"
        );
        assert!(!t("SEED", "invoice", "invoice.seed", &["gold"]));
        assert!(
            !t("FULL", "invoice", "invoice.scratch", &["gold"]),
            "explicit exclude wins"
        );

        // A single key: OR within it.
        let one = discover("from: sqlmesh\nstate: s\nwarehouse: w\nselect:\n  schemas: [a, b]\n");
        assert!(selected(&one, "FULL", "a", "a.x", &[]) && selected(&one, "FULL", "b", "b.y", &[]));
        assert!(!selected(&one, "FULL", "c", "c.z", &[]));

        let d2 = discover("from: sqlmesh\nstate: s\nwarehouse: w\nselect:\n  schemas: [invoice]\n  kinds: [EXTERNAL]\n");
        assert!(selected(&d2, "EXTERNAL", "invoice", "invoice.ext", &[]));
        assert!(!selected(&d2, "FULL", "invoice", "invoice.y", &[]));

        // The over-selection the beta hit: schemas + tags must intersect.
        let both = discover("from: sqlmesh\nstate: s\nwarehouse: w\nselect:\n  schemas: [ape_mktg]\n  tags: [adverity]\n");
        let adverity = ["adverity".to_string()];
        assert!(!selected(
            &both,
            "FULL",
            "ape_stg_adverity",
            "ape_stg_adverity.raw",
            &adverity
        ));
        assert!(selected(
            &both,
            "FULL",
            "ape_mktg",
            "ape_mktg.spend",
            &adverity
        ));
        assert!(!selected(
            &both,
            "FULL",
            "ape_mktg",
            "ape_mktg.fct_lead",
            &["qfai".to_string()]
        ));
    }

    #[test]
    fn names_are_mangled_and_collisions_are_loud() {
        assert_eq!(mangled_name("invoice.flight_spend"), "invoice_flight_spend");
        assert_eq!(mangled_name("Marts.Orders-Daily"), "marts_orders_daily");
        assert_eq!(mangled_name("1x.y"), "_1x_y");
        let d = discover("from: sqlmesh\nstate: s\nwarehouse: w\nselect:\n  schemas: [a]\n");
        let models = vec![model("a.b_c", "FULL", &[]), model("a_b.c", "FULL", &[])];
        let err = export_names(&d, &models).unwrap_err().to_string();
        assert!(
            err.contains("a.b_c") && err.contains("a_b.c") && err.contains("as:"),
            "{err}"
        );
        let d = discover(
            "from: sqlmesh\nstate: s\nwarehouse: w\nselect:\n  schemas: [a]\noverrides:\n  - model: a_b.c\n    as: other\n",
        );
        assert_eq!(export_names(&d, &models).unwrap(), vec!["a_b_c", "other"]);
    }

    #[test]
    fn materialize_binds_every_model_with_provenance_and_lineage() {
        let d = discover(
            "from: sqlmesh\nstate: s\nwarehouse: wh\nselect:\n  schemas: [inv]\noverrides:\n  - model: inv.spend\n    version: 2.1.0\n    contract: supported\n    grain: [month, id]\n    description: Authored wins.\n",
        );
        let mut parent = model("inv.flights", "VIEW", &[]);
        parent.description = Some("Flights.".to_string());
        parent.columns.insert(
            "id".to_string(),
            DeployedColumn {
                ty: "INT64".to_string(),
                description: Some("wh says".to_string()),
                description_source: Some(ColumnsSource::Warehouse),
            },
        );
        let mut child = model("inv.spend", "INCREMENTAL_BY_TIME_RANGE", &["invoicing"]);
        child.description = Some("Tool says.".to_string());
        child.grain = vec!["month".to_string()];
        child.depends_on = vec![
            "db.inv.flights".to_string(),
            "db.raw.unselected".to_string(),
        ];
        child.columns_source = ColumnsSource::Declared;
        child.columns.insert(
            "month".to_string(),
            DeployedColumn {
                ty: "DATE".to_string(),
                description: Some("Invoice month.".to_string()),
                description_source: Some(ColumnsSource::Declared),
            },
        );
        let catalog = DeployedCatalog {
            tool: "sqlmesh".to_string(),
            environment: "prod".to_string(),
            plan_id: "p1".to_string(),
            finalized_at: "2026-08-24T22:51:46Z".to_string(),
            synced_at: "2026-08-25T04:00:00Z".to_string(),
            evidence: Evidence::EnvironmentRow,
            schema_version: "100".to_string(),
            models: vec![parent, child],
            total_models: 3,
            unselected_models: 1,
        };
        let mut def: CellDef = serde_yaml::from_str("cell: c\n").unwrap();
        materialize(&mut def, &d, &catalog, false).unwrap();

        assert_eq!(def.interface.len(), 2);
        let flights = &def.interface[0];
        assert_eq!(flights.name, "inv_flights");
        assert_eq!(flights.version, "1.0.0");
        assert_eq!(flights.contract, Contract::Experimental);
        assert_eq!(flights.bind.as_deref(), Some("inv_flights"));
        assert_eq!(flights.from["description"], Origin::Sqlmesh);
        assert_eq!(flights.schema["id"].from["type"], Origin::Warehouse);
        assert_eq!(flights.schema["id"].from["description"], Origin::Warehouse);
        match &def.sources["inv_flights"] {
            Source::Connection {
                connection, table, ..
            } => {
                assert_eq!(connection, "wh");
                assert_eq!(table.as_deref(), Some("inv.flights"));
            }
            other => panic!("{other:?}"),
        }

        let spend = &def.interface[1];
        assert_eq!(spend.version, "2.1.0");
        assert_eq!(spend.contract, Contract::Supported);
        assert_eq!(spend.description.as_deref(), Some("Authored wins."));
        assert_eq!(spend.from["description"], Origin::CellYaml);
        assert_eq!(spend.grain, vec!["month", "id"]);
        assert_eq!(spend.from["grain"], Origin::CellYaml);
        assert_eq!(spend.schema["month"].from["type"], Origin::Sqlmesh);
        assert_eq!(spend.schema["month"].from["description"], Origin::Sqlmesh);
        let disc = spend.discovered.as_ref().unwrap();
        assert_eq!(disc.depends_on, vec!["inv_flights@1"]);
        assert_eq!(disc.depends_on_unselected, 1);
        assert_eq!(disc.at, "2026-08-25T04:00:00Z");
        assert_eq!(def.discovered_from.as_ref().unwrap().plan_id, "p1");

        // A catalog-addressing warehouse (BigQuery) keeps the model's own
        // catalog on the source, so `verify` reads it from there.
        let mut def2: CellDef = serde_yaml::from_str("cell: c\n").unwrap();
        materialize(&mut def2, &d, &catalog, true).unwrap();
        match &def2.sources["inv_flights"] {
            Source::Connection { table, .. } => {
                assert_eq!(table.as_deref(), Some("db.inv.flights"))
            }
            other => panic!("{other:?}"),
        }
    }
}
