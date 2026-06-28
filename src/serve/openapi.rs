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
                "schema": { "type": "integer", "maximum": 1000, "default": 100 } }),
        json!({ "name": "offset", "in": "query",
                "schema": { "type": "integer", "default": 0 } }),
    ];
    for g in &export.grain {
        params.push(json!({ "name": g, "in": "query", "schema": { "type": "string" } }));
    }

    let mut props = Map::new();
    for (col, ty) in &export.schema {
        props.insert(col.clone(), openapi_type(ty));
    }

    json!({
        "get": {
            "summary": format!("{} v{}", export.name, export.version),
            "parameters": params,
            "responses": {
                "200": {
                    "description": "rows",
                    "content": { "application/json": { "schema": {
                        "type": "array",
                        "items": { "type": "object", "properties": Value::Object(props) }
                    }}}
                }
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
        _ => json!({ "type": "string" }),
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
        // Unknown types degrade to string rather than erroring.
        assert_eq!(openapi_type("blob"), json!({ "type": "string" }));
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
    }
}
