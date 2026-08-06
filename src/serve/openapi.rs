use crate::config::{CellDef, Export, Visibility};
use serde_json::{json, Map, Value};

/// Generate an OpenAPI 3.1 document directly from the cell interface — the
/// interface is the single source of truth, so the spec is derived, never
/// hand-annotated.
pub fn generate(def: &CellDef) -> Value {
    let mut paths = Map::new();
    for export in &def.interface {
        if export.visibility != Visibility::Discoverable {
            continue;
        }
        if let Ok(route) = export.route() {
            paths.insert(format!("/{route}"), path_item(export));
        }
    }
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": def.cell,
            "version": "0.0.0",
            "description": "Generated from the cell interface"
        },
        "paths": Value::Object(paths)
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
            .map(|ty| openapi_type(ty))
            .unwrap_or_else(|| json!({}));
        params.push(json!({ "name": g, "in": "query",
                            "description": "Grain filter — exact equality only.",
                            "schema": schema }));
    }

    let mut props = Map::new();
    for (col, ty) in &export.schema {
        props.insert(col.clone(), openapi_type(ty));
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
    use crate::config::{Contract, Export, Visibility};
    use indexmap::IndexMap;

    fn export_with(name: &str, version: &str, visibility: Visibility) -> Export {
        let mut schema = IndexMap::new();
        schema.insert("order_date".to_string(), "date".to_string());
        schema.insert("revenue".to_string(), "decimal".to_string());
        Export {
            name: name.to_string(),
            version: version.to_string(),
            source: None,
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
            sources: IndexMap::new(),
            transforms: vec![],
            interface: vec![
                export_with("orders_daily", "2.1.0", Visibility::Discoverable),
                export_with("internal", "1.0.0", Visibility::Private),
            ],
            access: Default::default(),
        };
        let doc = generate(&def);
        assert_eq!(doc["openapi"], "3.1.0");
        assert_eq!(doc["info"]["title"], "orders");

        let paths = doc["paths"].as_object().unwrap();
        // Discoverable export is routed on its major version; private one is omitted.
        assert!(paths.contains_key("/orders_daily@2"));
        assert!(!paths.contains_key("/internal@1"));
        assert_eq!(paths.len(), 1);

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
}
