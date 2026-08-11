use crate::config::Export;
use serde_json::{json, Map, Value};

/// Generate an OpenAPI 3.1 document from the shared visibility-filtered
/// route list (`context::discoverable_routes` — ADR 0012 §4: one list, never
/// an independent re-application of the predicate). The interface is the
/// single source of truth, so the spec is derived, never hand-annotated.
/// `version` is the context document's interface digest (ADR 0012 §7) — it
/// moves when the interface moves, not when data refreshes under it.
/// `description` is the cell's declared one-liner (ADR 0012 §3) when the
/// author wrote one.
///
/// Meta path items (`/`, `/context`, `/openapi.json`) are **always**
/// emitted, `routes` or not — the honesty fix ADR 0013 §8 makes in the same
/// change: before this, the spec described only data paths, so `/context`
/// and `/openapi.json` appeared nowhere and `--no-data` served `"paths": {}`
/// while three routes were live. Data path items stay gated on `routes`
/// (empty under `--no-data`, since those affordances genuinely don't exist
/// there).
pub fn generate(
    cell: &str,
    description: Option<&str>,
    routes: &[(String, Export)],
    version: &str,
) -> Value {
    let mut paths = Map::new();
    paths.insert("/".to_string(), health_path_item());
    paths.insert("/context".to_string(), context_path_item());
    paths.insert("/openapi.json".to_string(), openapi_path_item());
    for (route, export) in routes {
        paths.insert(format!("/{route}"), path_item(export));
    }
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": cell,
            "version": version,
            "description": description.unwrap_or("Generated from the cell interface")
        },
        "paths": Value::Object(paths)
    })
}

fn health_path_item() -> Value {
    json!({
        "get": {
            "summary": "Liveness",
            "description": "Pre-auth liveness: cell name, status, and (published mode) the \
                            served execution number.",
            "responses": {
                "200": { "description": "cell + status; execution present in published mode" }
            }
        }
    })
}

/// `/context`'s `include` parameter is documented from `INCLUDE_SECTIONS`
/// (`serve::INCLUDE_SECTIONS`) — the exact vocabulary `validate_include`
/// enforces, so the two can never drift (ADR 0013 §8).
fn context_path_item() -> Value {
    let sections: Vec<Value> = super::INCLUDE_SECTIONS.iter().map(|s| json!(s)).collect();
    json!({
        "get": {
            "summary": "The cell's context document (ADR 0012)",
            "description": "The interface made machine-readable: declared schema/grain/meaning, \
                            the query grammar, and (once built) provenance and measurements.",
            "parameters": [{
                "name": "include",
                "in": "query",
                "required": false,
                "style": "form",
                "explode": false,
                "description": "Comma-separated optional sections to inline. Omit for the \
                                default document; `docs` inlines every declared docs page.",
                "schema": { "type": "array", "items": { "type": "string", "enum": sections } }
            }],
            "responses": {
                "200": { "description": "the context document" },
                "304": { "description": "not modified (If-None-Match matched the current ETag \
                                          for the requested variant)" },
                "400": { "description": "unknown query parameter, or an unrecognized/empty \
                                          `include` section" },
                "401": { "description": "missing or unknown bearer token (cell has access.roles)" },
                "403": { "description": "cell is not shareable, or the token's roles do not \
                                          include an allowed role" }
            }
        }
    })
}

fn openapi_path_item() -> Value {
    json!({
        "get": {
            "summary": "This document",
            "responses": {
                "200": { "description": "the OpenAPI document" },
                "401": { "description": "missing or unknown bearer token (cell has access.roles)" },
                "403": { "description": "cell is not shareable, or the token's roles do not \
                                          include an allowed role" }
            }
        }
    })
}

fn path_item(export: &Export) -> Value {
    let mut params = vec![
        json!({ "name": "limit", "in": "query",
                "description": "Rows per page; values above the maximum are clamped.",
                "schema": { "type": "integer",
                            "maximum": super::MAX_LIMIT,
                            "default": super::DEFAULT_LIMIT } }),
        json!({ "name": "offset", "in": "query",
                "description": "Rows to skip; requests beyond the maximum are rejected (400).",
                "schema": { "type": "integer",
                            "maximum": super::MAX_OFFSET,
                            "default": 0 } }),
    ];
    for g in &export.grain {
        // ADR 0012 §7: grain params are typed from the declared schema, not
        // hardcoded `string`. A grain column with no declared type gets an
        // empty schema — no claim — never a fabricated one.
        let schema = export
            .schema
            .get(g)
            .map(|spec| openapi_type(&spec.ty))
            .unwrap_or_else(|| json!({}));
        params.push(json!({ "name": g, "in": "query",
                            "description": "Grain filter — exact equality only.",
                            "schema": schema }));
    }

    let mut props = Map::new();
    for (col, spec) in &export.schema {
        let mut prop = openapi_type(&spec.ty);
        // The meaning fields (ADR 0012 §3) ride the standard `description`
        // slot plus an x- extension for the structured unit — the context
        // document stays the primary surface; this keeps the two consistent.
        if let Some(d) = &spec.description {
            prop["description"] = json!(d);
        }
        if let Some(u) = &spec.unit {
            prop["x-datamk-unit"] = json!(u);
        }
        props.insert(col.clone(), prop);
    }

    json!({
        "get": {
            "summary": format!("{} v{}", export.name, export.version),
            "parameters": params,
            // The real response surface (ADR 0012 §7) — what serve_export and
            // authorize() actually return, not just the happy path.
            "responses": {
                "200": {
                    "description": "rows",
                    "content": { "application/json": { "schema": {
                        "type": "array",
                        "items": { "type": "object", "properties": Value::Object(props) }
                    }}}
                },
                "400": { "description": "unknown or invalid query parameter (only grain columns, limit, and offset are accepted — exact equality only)" },
                "401": { "description": "missing or unknown bearer token (cell has access.roles)" },
                "403": { "description": "cell is not shareable, or the token's roles do not include an allowed role" },
                "404": { "description": "no such export route" },
                "500": { "description": "query execution failed" }
            }
        }
    })
}

fn openapi_type(ty: &str) -> Value {
    match ty.to_lowercase().as_str() {
        "string" | "varchar" | "text" => json!({ "type": "string" }),
        "int" | "integer" | "bigint" | "long" => json!({ "type": "integer" }),
        "decimal" | "numeric" | "double" | "float" => json!({ "type": "number" }),
        "bool" | "boolean" => json!({ "type": "boolean" }),
        "date" => json!({ "type": "string", "format": "date" }),
        "timestamp" => json!({ "type": "string", "format": "date-time" }),
        // ADR 0012 §7: an unknown declared type must not degrade to a
        // fabricated `string` — emit no type claim at all, and carry the
        // declared name so a reader can still see what the author wrote.
        _ => json!({ "x-datamk-declared-type": ty }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CellDef, ColumnSpec, Contract, Export, Visibility};
    use indexmap::IndexMap;

    fn export_with(name: &str, version: &str, visibility: Visibility) -> Export {
        let mut schema = IndexMap::new();
        schema.insert("order_date".to_string(), ColumnSpec::bare("date"));
        schema.insert("revenue".to_string(), ColumnSpec::bare("decimal"));
        Export {
            name: name.to_string(),
            version: version.to_string(),
            source: None,
            bind: None,
            description: None,
            docs: None,
            grain: vec!["order_date".to_string()],
            schema,
            freshness: None,
            visibility,
            contract: Contract::Experimental,
        }
    }

    #[test]
    fn openapi_type_maps_known_and_unknown_types() {
        assert_eq!(openapi_type("string"), json!({ "type": "string" }));
        assert_eq!(openapi_type("BIGINT"), json!({ "type": "integer" }));
        assert_eq!(openapi_type("decimal"), json!({ "type": "number" }));
        assert_eq!(openapi_type("boolean"), json!({ "type": "boolean" }));
        assert_eq!(
            openapi_type("date"),
            json!({ "type": "string", "format": "date" })
        );
        assert_eq!(
            openapi_type("timestamp"),
            json!({ "type": "string", "format": "date-time" })
        );
        // ADR 0012 §7: unknown types emit no type claim — never a fabricated
        // `string` — and carry the declared name for the reader.
        assert_eq!(
            openapi_type("blob"),
            json!({ "x-datamk-declared-type": "blob" })
        );
    }

    #[test]
    fn generate_emits_a_path_per_discoverable_export() {
        let def = CellDef {
            cell: "orders".to_string(),
            description: None,
            docs: None,
            sources: IndexMap::new(),
            transforms: vec![],
            interface: vec![
                export_with("orders_daily", "2.1.0", Visibility::Discoverable),
                export_with("internal", "1.0.0", Visibility::Private),
            ],
            access: Default::default(),
        };
        // The same shared route list `serve` and `/context` read (ADR 0012 §4).
        let routes = crate::context::discoverable_routes(&def).unwrap();
        let doc = generate(&def.cell, def.description.as_deref(), &routes, "digest123");
        assert_eq!(doc["openapi"], "3.1.0");
        assert_eq!(doc["info"]["title"], "orders");
        // ADR 0012 §7: the version is the interface digest, not "0.0.0".
        assert_eq!(doc["info"]["version"], "digest123");

        let paths = doc["paths"].as_object().unwrap();
        // Discoverable export is routed on its major version; private one is omitted.
        assert!(paths.contains_key("/orders_daily@2"));
        assert!(!paths.contains_key("/internal@1"));
        // ADR 0013 §8: meta paths are always emitted alongside the one
        // discoverable data path.
        assert!(paths.contains_key("/"));
        assert!(paths.contains_key("/context"));
        assert!(paths.contains_key("/openapi.json"));
        assert_eq!(paths.len(), 4, "{:?}", paths.keys().collect::<Vec<_>>());

        let params = doc["paths"]["/orders_daily@2"]["get"]["parameters"]
            .as_array()
            .unwrap();
        let names: Vec<&str> = params.iter().map(|p| p["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"limit"));
        assert!(names.contains(&"offset"));
        // Grain columns become query params.
        assert!(names.contains(&"order_date"));

        // ADR 0012 §7: the grain param is typed from the declared schema
        // (`order_date: date`), not hardcoded `string`.
        let order_date = params.iter().find(|p| p["name"] == "order_date").unwrap();
        assert_eq!(
            order_date["schema"],
            json!({ "type": "string", "format": "date" })
        );

        // The real error surface is documented alongside the rows.
        let responses = &doc["paths"]["/orders_daily@2"]["get"]["responses"];
        for code in ["200", "400", "401", "403", "404", "500"] {
            assert!(responses.get(code).is_some(), "missing response {code}");
        }
    }

    #[test]
    fn grain_param_without_a_declared_type_makes_no_type_claim() {
        let mut e = export_with("orders_daily", "2.1.0", Visibility::Discoverable);
        e.grain = vec!["undeclared_col".to_string()];
        let item = path_item(&e);
        let params = item["get"]["parameters"].as_array().unwrap();
        let p = params
            .iter()
            .find(|p| p["name"] == "undeclared_col")
            .unwrap();
        assert_eq!(p["schema"], json!({}));
    }

    /// ADR 0013 §8: meta paths are emitted even with zero data routes
    /// (`--no-data`, or a cell with no discoverable exports) — the honesty
    /// bug this ADR fixes: before, `--no-data` served `"paths": {}` while
    /// `/`, `/context`, `/openapi.json` were all live.
    #[test]
    fn generate_emits_meta_paths_with_zero_data_routes() {
        let doc = generate("orders", None, &[], "digest123");
        let paths = doc["paths"].as_object().unwrap();
        let keys: std::collections::BTreeSet<&str> = paths.keys().map(|k| k.as_str()).collect();
        let expected: std::collections::BTreeSet<&str> =
            ["/", "/context", "/openapi.json"].into_iter().collect();
        assert_eq!(keys, expected);
    }

    /// ADR 0013 §8: `/context`'s `include` parameter is documented from the
    /// same vocabulary constant `validate_include` enforces — a fixture in
    /// the mold of `context_query_block_claims_match_the_enforced_grammar`,
    /// so a change to either fails loudly.
    #[test]
    fn context_include_param_is_generated_from_the_shared_vocabulary() {
        let doc = generate("orders", None, &[], "digest123");
        let params = doc["paths"]["/context"]["get"]["parameters"]
            .as_array()
            .unwrap();
        assert_eq!(params.len(), 1);
        let include = &params[0];
        assert_eq!(include["name"], "include");
        assert_eq!(include["style"], "form");
        assert_eq!(include["explode"], false);
        let enumerated: Vec<&str> = include["schema"]["items"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(enumerated, super::super::INCLUDE_SECTIONS.to_vec());

        let responses = &doc["paths"]["/context"]["get"]["responses"];
        for code in ["200", "304", "400", "401", "403"] {
            assert!(responses.get(code).is_some(), "missing response {code}");
        }
    }
}
