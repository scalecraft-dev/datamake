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
