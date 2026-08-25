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
    base_path: &str,
) -> Value {
    let mut paths = Map::new();
    paths.insert("/".to_string(), health_path_item());
    paths.insert("/context".to_string(), context_path_item());
    paths.insert("/openapi.json".to_string(), openapi_path_item());
    for (route, export) in routes {
        paths.insert(format!("/{route}"), path_item(export));
    }
    let mut doc = json!({
        "openapi": "3.1.0",
        "info": {
            "title": cell,
            "version": version,
            "description": description.unwrap_or("Generated from the cell interface")
        },
        "paths": Value::Object(paths)
    });
    // A mounted cell (ADR 0014) declares its base in `servers`, never by
    // rewriting path keys: those are derived from route keys, which are
    // contract (`name@major`), and prefixing them would fold the deployment's
    // mount point into a contract-derived identifier.
    if !base_path.is_empty() {
        doc["servers"] = json!([{ "url": base_path }]);
    }
    doc
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
                                default document; `docs` inlines every declared docs page. \
                                Each section named in the response's `included` array is \
                                inlined ON THE RECORDS it belongs to — `include=docs` echoes \
                                `\"included\": [\"docs\"]` and sets `content` on each \
                                `docs[]` entry. `included` holds section names, never the \
                                content itself.",
                "schema": { "type": "array", "items": { "type": "string", "enum": sections } }
            }],
            "responses": {
                "200": {
                    "description": "the context document",
                    "content": { "application/json": { "schema": context_schema() } }
                },
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

/// The context document's response shape (ADR 0015: flat, per-field
/// provenance). Hand-written against `context::ContextDocument` and pinned
/// to it by `context_schema_names_every_top_level_document_key` — the
/// `/context` 200 used to be a bare description string, so nothing in the
/// spec said where `include=` content lands or that `binding` exists.
fn context_schema() -> Value {
    json!({
        "type": "object",
        "required": ["datamk_context", "cell", "status", "grain_verified", "exports",
                     "upstreams", "docs", "include_request", "data", "notes", "included"],
        "description": "One level, no regions. A fact is a CLAIM iff its record carries \
                        `from` (who said it); a fact is a MEASUREMENT iff it sits in a \
                        block with a timestamp (`build`, `source_check`, `freshness`, an \
                        export's `probe`/`check`). Absent facts are omitted, never \
                        fabricated.",
        "properties": {
            "datamk_context": {
                "type": "integer",
                "const": crate::context::DATAMK_CONTEXT_VERSION,
                "description": "Document-schema version."
            },
            "cell": { "type": "string" },
            "status": {
                "type": "string",
                "enum": ["draft", "verified_at_source", "verified"],
                "description": "Weakest to strongest. `verified` means a published, \
                                verify-gated execution stands behind this document."
            },
            "grain_verified": { "type": "boolean" },
            "description": { "type": "string" },
            "from": from_schema("description"),
            "discovered_from": {
                "type": "object",
                "description": "Present iff the interface was discovered from a modeling \
                                tool's deployed state (ADR 0016): which tool, environment, \
                                plan, and how \"deployed\" is evidenced.",
                "properties": {
                    "tool": { "type": "string" },
                    "environment": { "type": "string" },
                    "plan_id": { "type": "string" },
                    "finalized_at": { "type": "string", "format": "date-time" },
                    "synced_at": { "type": "string", "format": "date-time" },
                    "evidence": { "type": "string", "enum": ["environment_row", "artifact_only"] }
                }
            },
            "exports": { "type": "array", "items": export_schema() },
            "upstreams": { "type": "array", "items": { "type": "object",
                "required": ["ref"],
                "properties": {
                    "ref": { "type": "string" },
                    "version": { "type": "integer", "description": "The author's pin." },
                    "execution": { "type": "integer",
                        "description": "What the Builder actually attached — a measurement." },
                    "data_as_of": { "type": "string", "format": "date-time" }
                }}},
            "docs": { "type": "array", "items": { "type": "object",
                "required": ["target", "source_path", "media_type"],
                "properties": {
                    "target": { "type": "string",
                                "description": "`cell`, or an export's route key." },
                    "source_path": { "type": "string",
                        "description": "The author's cell.yaml-relative file path. \
                                        NOT a URL and not fetchable — there is no \
                                        /docs route; use ?include=docs for content." },
                    "media_type": { "type": "string" },
                    "sha256": { "type": "string",
                                "description": "Release-time fingerprint; absent until released." },
                    "bytes": { "type": "integer" },
                    "content": { "type": "string",
                        "description": "Present only when `included` contains `docs`." }
                }}},
            "include_request": { "type": "string" },
            "build": {
                "type": "object",
                "description": "Provenance of the published execution behind this document. \
                                Absent when none stands behind it.",
                "properties": {
                    "execution": { "type": "integer" },
                    "snapshot_id": { "type": ["integer", "null"] },
                    "verify_outcome": { "type": "string" },
                    "started_at": { "type": "string", "format": "date-time" },
                    "finished_at": { "type": "string", "format": "date-time" },
                    "datamk_version": { "type": "string" },
                    "data_as_of": { "type": "string" }
                }
            },
            "source_check": {
                "type": "object",
                "description": "A live check of the bound exports against their declared \
                                sources. Per-export measurements ride `exports[].check`.",
                "properties": {
                    "outcome": { "type": "string" },
                    "checked_at": { "type": "string", "format": "date-time" },
                    "data_as_of": { "type": "string", "format": "date-time" },
                    "datamk_version": { "type": "string" }
                }
            },
            "freshness": { "type": "object" },
            "data": {
                "type": "object",
                "required": ["served_here", "channels"],
                "properties": {
                    "served_here": { "type": "boolean",
                        "description": "False when this endpoint serves no rows — see each \
                                        export's `binding` for where they are." },
                    "channels": { "type": "array", "items": { "type": "string" },
                        "description": "Operator hints from the profile. Free-form prose; \
                                        `exports[].binding` is the machine-readable target." }
                }
            },
            "notes": { "type": "array", "items": { "type": "string" } },
            "included": {
                "type": "array",
                "items": { "type": "string", "enum": super::INCLUDE_SECTIONS.iter()
                                                        .map(|s| json!(s))
                                                        .collect::<Vec<Value>>() },
                "description": "Section NAMES inlined by this response, never their content. \
                                `docs` means each `docs[]` entry carries `content`."
            }
        }
    })
}

/// The `from` map (ADR 0015 §2): field name -> origin, for every field of
/// the record that can originate in more than one place.
fn from_schema(fields: &str) -> Value {
    json!({
        "type": "object",
        "description": format!("Origin of each claim on this record ({fields}). Closed set."),
        "additionalProperties": { "type": "string", "enum": ["cell.yaml", "warehouse", "sqlmesh"] }
    })
}

fn export_schema() -> Value {
    json!({
        "type": "object",
        "required": ["name", "version", "route", "contract", "grain", "schema"],
        "properties": {
            "name": { "type": "string" },
            "version": { "type": "string", "description": "Semver. The route keys on MAJOR." },
            "route": { "type": "string",
                "description": "The route key (`name@major`) — this export's identity and its \
                                docs `target`. It is also the HTTP path only when `query` is \
                                non-null; a bound export has no path." },
            "contract": { "type": "string", "enum": ["experimental", "supported"] },
            "description": { "type": "string" },
            "freshness": { "type": "string" },
            "grain": { "type": "array", "items": { "type": "string" } },
            "from": from_schema("description, grain"),
            "schema": { "type": "object", "additionalProperties": { "type": "object",
                "properties": {
                    "type": { "type": "string" },
                    "unit": { "type": "string" },
                    "description": { "type": "string" },
                    "from": from_schema("type, unit, description")
                }}},
            "query": {
                "type": ["object", "null"],
                "description": "The served query grammar. Null iff this export is bound — the \
                                machine-checkable signal that `GET /{route}` does not exist."
            },
            "binding": {
                "type": "object",
                "description": "Where the rows are, for a bound export. Present iff `query` \
                                is null. Values are verbatim cell.yaml — never \
                                profile-resolved, so a templated table ships as written.",
                "required": ["source"],
                "properties": {
                    "source": { "type": "string", "description": "The `sources:` key." },
                    "object": { "type": "string",
                                "description": "The declared warehouse object, or file/glob." },
                    "connection": { "type": "string",
                                    "description": "Connection alias; what it resolves to is \
                                                    profile, not contract." }
                }
            },
            "depends_on": { "type": "array", "items": { "type": "string" },
                "description": "Route keys of this export's selected parents within the cell \
                                (discovered cells)." },
            "depends_on_unselected": { "type": "integer",
                "description": "Parents left out by selection — a count, never names." },
            "deployed": {
                "type": "object",
                "description": "What the modeling tool says about this model's deployment \
                                (kind, cron, owner, tags, fingerprint, loaded intervals). \
                                Measured at `at`; outside the digest.",
                "required": ["at", "model", "kind", "fingerprint"],
                "properties": { "at": { "type": "string", "format": "date-time" } }
            },
            "probe": {
                "type": "object",
                "description": "Swap-time measurement against the rows this route serves \
                                (rows, coverage, values, example_request). Absent until measured.",
                "required": ["at"],
                "properties": { "at": { "type": "string", "format": "date-time" } }
            },
            "check": {
                "type": "object",
                "description": "What `datamk verify`'s live check measured for this export.",
                "required": ["at", "check", "grain", "rows", "distinct_grain"],
                "properties": {
                    "at": { "type": "string", "format": "date-time" },
                    "check": { "type": "string", "enum": ["grain_unique"] },
                    "grain": { "type": "array", "items": { "type": "string" } },
                    "rows": { "type": "integer" },
                    "distinct_grain": { "type": "integer" }
                }
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
        // The version and contract used to exist only as prose inside
        // `summary` — unreadable to the machines this document is for.
        "x-datamk-version": export.version,
        "x-datamk-contract": export.contract,
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
            from: Default::default(),
            discovered: None,
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
            discover: None,
            discovered_from: None,
        };
        // The same shared route list `serve` and `/context` read (ADR 0012 §4).
        let routes = crate::context::discoverable_routes(&def).unwrap();
        let doc = generate(
            &def.cell,
            def.description.as_deref(),
            &routes,
            "digest123",
            "",
        );
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

    /// The `/context` 200 used to be a bare description string, so nothing in
    /// the spec said the document had a `docs` key, a `binding`, or where
    /// `include=` content lands. Pinned to `ContextDocument`'s own field list
    /// so a new top-level key can't ship undocumented.
    #[test]
    fn context_schema_names_every_top_level_document_key() {
        let doc = generate("orders", None, &[], "digest123", "");
        let schema = &doc["paths"]["/context"]["get"]["responses"]["200"]["content"]
            ["application/json"]["schema"];
        let props = schema["properties"].as_object().unwrap();
        for key in [
            "datamk_context",
            "cell",
            "status",
            "grain_verified",
            "description",
            "from",
            "discovered_from",
            "exports",
            "upstreams",
            "docs",
            "include_request",
            "build",
            "source_check",
            "freshness",
            "data",
            "notes",
            "included",
        ] {
            assert!(props.contains_key(key), "undocumented top-level key {key}");
        }
        assert_eq!(
            schema["properties"]["datamk_context"]["const"],
            json!(crate::context::DATAMK_CONTEXT_VERSION)
        );
        // The two fields the reported round-trips were lost on.
        let export = &props["exports"]["items"]["properties"];
        assert!(export.get("binding").is_some());
        assert!(export.get("probe").is_some() && export.get("check").is_some());
        assert!(
            export.get("from").is_some(),
            "per-field provenance is documented"
        );
        let docs_entry = &props["docs"]["items"]["properties"];
        assert!(
            docs_entry.get("content").is_some(),
            "the include=docs landing spot"
        );
        assert!(docs_entry.get("source_path").is_some());
        assert!(
            docs_entry.get("path").is_none(),
            "the v1 name must not linger in the spec"
        );
        // The include= landing rule an agent otherwise learns by trial.
        let include_desc = doc["paths"]["/context"]["get"]["parameters"][0]["description"]
            .as_str()
            .unwrap();
        assert!(include_desc.contains("ON THE RECORDS"), "{include_desc}");
    }

    /// Version and contract used to exist only as prose inside `summary`.
    #[test]
    fn data_path_items_carry_version_and_contract_as_extensions() {
        let def = CellDef {
            cell: "orders".to_string(),
            description: None,
            docs: None,
            sources: IndexMap::new(),
            transforms: vec![],
            interface: vec![export_with(
                "orders_daily",
                "2.1.0",
                Visibility::Discoverable,
            )],
            access: Default::default(),
            discover: None,
            discovered_from: None,
        };
        let routes = crate::context::discoverable_routes(&def).unwrap();
        let doc = generate(&def.cell, None, &routes, "digest123", "");
        let item = &doc["paths"]["/orders_daily@2"];
        assert_eq!(item["x-datamk-version"], "2.1.0");
        assert_eq!(item["x-datamk-contract"], "experimental");
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
        let doc = generate("orders", None, &[], "digest123", "");
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
        let doc = generate("orders", None, &[], "digest123", "");
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

    /// A non-empty `base_path` adds `servers` and leaves path keys alone
    /// (ADR 0014) — the mount lives in `servers[0].url`, never folded into a
    /// contract-derived route key.
    #[test]
    fn base_path_adds_servers_without_touching_path_keys() {
        let doc = generate("orders", None, &[], "digest123", "/weather");
        assert_eq!(doc["servers"], json!([{ "url": "/weather" }]));
        assert!(doc["paths"].as_object().unwrap().contains_key("/context"));

        let unmounted = generate("orders", None, &[], "digest123", "");
        assert!(unmounted.get("servers").is_none(), "{unmounted}");
    }
}
