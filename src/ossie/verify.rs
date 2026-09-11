//! `datamk verify` checks against a bound Ossie semantic model (ADR 0018
//! §6). Every check here is plan-time only — `DESCRIBE` resolves
//! identifiers and types without reading a row (ADR 0018 premise 1) — so a
//! bad claim fails `datamk verify`/`datamk run`'s auto-verify exactly like a
//! bad declared column does, never by executing the metric or the
//! relationship's join.

use anyhow::{bail, Result};
use duckdb::Connection;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::bind::SemanticIndex;
use super::{Dataset, Dialect, SemanticModel};
use crate::config::{CellDef, Export};

/// One dataset's check, keyed `"model/dataset@route"` (or `"model/dataset"`
/// when unbound — `route` is `None`, `dataset_key` omits the suffix) in
/// `SemanticCheckRecord::datasets` (`record.rs`). A dataset bound to two
/// routes (two majors of one export name) produces two entries, one per
/// route — keying on `"model/dataset"` alone would let the second binding's
/// check silently overwrite the first's. `model`/`dataset` are not part of
/// the wire shape — the map key already carries them — but `check_export`
/// needs somewhere to put them so `verify::check` can build that key
/// without a second lookup.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DatasetCheck {
    #[serde(skip)]
    pub model: String,
    #[serde(skip)]
    pub dataset: String,
    /// `None` for a dataset ADR 0018 §5 kept but bound to nothing.
    pub route: Option<String>,
    /// `"matches"` | `"no_grain"` | `"absent"` for a bound dataset;
    /// `"unbound"` when `route` is `None` — nothing was compared; `"error"`
    /// when `primary_key` genuinely disagrees with the export's grain (the
    /// message rides in `check_export`'s `errors` accumulator, follow-up 8
    /// — the failing claim, never a bailed-out call).
    pub primary_key: String,
    pub fields: BTreeMap<String, FieldCheck>,
}

/// The `SemanticCheckRecord::datasets` key for one binding of `model/dataset`
/// — one entry per bound route, so a dataset bound to two majors records
/// both (ADR 0018 §5). `route: None` (unbound, kept per §5) omits the
/// suffix, matching the model-level key shape `relationships`/`metrics` use.
pub fn dataset_key(model: &str, dataset: &str, route: Option<&str>) -> String {
    match route {
        Some(r) => format!("{model}/{dataset}@{r}"),
        None => format!("{model}/{dataset}"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldCheck {
    pub verified: bool,
    /// `"dialect"` (no `ANSI_SQL` variant), `"function"` (`DESCRIBE`
    /// rejected the expression because DuckDB has no such scalar/aggregate/
    /// macro — a foreign-dialect callable authored under `ANSI_SQL`, e.g.
    /// BigQuery's `HLL_COUNT.MERGE`), or `"error"` (a genuine hard-failing
    /// claim: a bad column, or DuckDB rejected the expression for any other
    /// reason — follow-up 8: recorded, not bailed out on, so the rest of
    /// this dataset/export is still checked; the message rides in
    /// `check_export`'s `errors` accumulator).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What one `verify::check` pass measured against the whole bound semantic
/// model — the payload `SemanticCheckRecord` persists.
#[derive(Debug, Clone, Default)]
pub struct SemanticOutcome {
    pub datasets: BTreeMap<String, DatasetCheck>,
    pub relationships: BTreeMap<String, String>,
    pub metrics: BTreeMap<String, String>,
}

/// Double-quote an identifier, escaping any embedded `"` — the same rule
/// `verify::quote_ident` applies to a census alias, repeated here rather
/// than exported (the two build sites have nothing else in common).
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `DESCRIBE <sql>`, executed (not merely prepared): a `DESCRIBE` plans the
/// inner `SELECT` and returns its column list without reading a row, but
/// DuckDB only raises a binder error once the statement is actually run —
/// `prepare` alone does not catch every case `query_map` does. Returns the
/// raw `duckdb::Error` so callers quote DuckDB's own message verbatim.
fn describe_plan(conn: &Connection, sql: &str) -> std::result::Result<(), duckdb::Error> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |_| Ok(()))?;
    for r in rows {
        r?;
    }
    Ok(())
}

const NOTHING_EXECUTED: &str =
    "Nothing was executed: this is a plan-time check, no rows were read.";

/// Whether a `describe_plan` failure names a missing function rather than a
/// bad column (follow-up 1: a 46-dataset SQLMesh estate's real trigger,
/// `HLL_COUNT.MERGE(x)` — a BigQuery-native aggregate authored under
/// `ANSI_SQL` because OSI 0.1.1's dialect enum has no `BIGQUERY`). DuckDB's
/// catalog error names the callable's kind ("Scalar Function", "Aggregate
/// Function", "Macro") followed by "with name ... does not exist"; matched
/// loosely (contains both "function" and "does not exist", ASCII
/// case-insensitive) rather than against one exact wording, since the kind
/// prefix varies. A missing *column* says "does not have a column named" and
/// never matches this — it stays a hard error.
fn is_missing_function_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("function") && lower.contains("does not exist")
}

/// H2: what `assert_scalar_or_aggregate_shape` refuses — a foreign
/// expression interpolated raw into `DESCRIBE SELECT <expr> FROM …` binds
/// (not merely parses) whatever it names, so a subquery or table function
/// would be *executed enough* to read or sniff a file
/// (`read_csv('/etc/passwd')`, `read_parquet('s3://…')`) at plan-check time.
const NOT_SCALAR_OR_AGGREGATE: &str = "expressions are scalar or aggregate over the dataset's \
     columns; subqueries and table functions are refused";

/// A shape gate applied to every field/metric `ANSI_SQL` expression
/// *before* it is interpolated into `DESCRIBE SELECT <expr> FROM …` (H2):
/// refuses `;` (multiple statements — belt-and-braces; `describe_plan`'s
/// own `conn.prepare` already rejects multi-statement text, see
/// `verify::tests::duckdb_prepare_rejects_multi_statement_text`), the
/// keywords `SELECT`/`FROM`/`WITH` as standalone tokens, and a call
/// (identifier immediately followed by `(`) to any function whose name
/// starts with `read_`, `sniff_`, `parquet_`, `glob`, `query`, `httpfs`, or
/// ends with `_scan`. Token-based, not a real SQL parse — `describe_plan`
/// is still the check that resolves identifiers and types; this only
/// refuses shapes that must never reach DuckDB's binder in the first place.
fn assert_scalar_or_aggregate_shape(expr: &str) -> Result<()> {
    const REFUSED_KEYWORDS: [&str; 3] = ["select", "from", "with"];
    const REFUSED_FUNCTION_PREFIXES: [&str; 6] =
        ["read_", "sniff_", "parquet_", "glob", "query", "httpfs"];
    const REFUSED_FUNCTION_SUFFIX: &str = "_scan";

    if expr.contains(';') {
        bail!("{NOT_SCALAR_OR_AGGREGATE} (found ';').");
    }

    let chars: Vec<(usize, char)> = expr.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (start, c) = chars[i];
        if c.is_ascii_alphabetic() || c == '_' {
            let mut j = i + 1;
            while j < chars.len() {
                let (_, c) = chars[j];
                if c.is_ascii_alphanumeric() || c == '_' {
                    j += 1;
                } else {
                    break;
                }
            }
            let end = if j < chars.len() {
                chars[j].0
            } else {
                expr.len()
            };
            let token = &expr[start..end];
            let lower = token.to_ascii_lowercase();
            if REFUSED_KEYWORDS.contains(&lower.as_str()) {
                bail!("{NOT_SCALAR_OR_AGGREGATE} (found `{token}`).");
            }
            let mut k = j;
            while k < chars.len() && chars[k].1.is_whitespace() {
                k += 1;
            }
            if k < chars.len() && chars[k].1 == '(' {
                let is_refused = REFUSED_FUNCTION_PREFIXES
                    .iter()
                    .any(|p| lower.starts_with(p))
                    || lower.ends_with(REFUSED_FUNCTION_SUFFIX);
                if is_refused {
                    bail!("{NOT_SCALAR_OR_AGGREGATE} (found a call to `{token}(…)`).");
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    Ok(())
}

/// The trimmed expression, if (and only if) it is a bare identifier —
/// unquoted (`^[A-Za-z_][A-Za-z0-9_]*$`) or a single `"..."`-quoted token
/// with `""` as its only permitted embedded quote. Anything else (a
/// dotted/qualified name, a function call, a literal, an operator) is not
/// an identity expression and returns `None`.
fn bare_identifier(expr: &str) -> Option<String> {
    let e = expr.trim();
    if e.len() >= 2 && e.starts_with('"') && e.ends_with('"') {
        let inner = &e[1..e.len() - 1];
        let unescaped = inner.replace("\"\"", "\"");
        if unescaped.is_empty() {
            return None;
        }
        return Some(unescaped);
    }
    let mut chars = e.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    if !e
        .chars()
        .skip(1)
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    Some(e.to_string())
}

/// The `ANSI_SQL` dialect variant of an expression, when one exists — the
/// only dialect datamk ever plan-checks (ADR 0018 §6) or surfaces as
/// `fields[].expression`/`metrics[].expression` in the context document
/// (ADR 0018 §7).
pub fn ansi_sql(expr: &super::Expression) -> Option<&str> {
    expr.dialects
        .iter()
        .find(|d| d.dialect == Dialect::AnsiSql)
        .map(|d| d.expression.as_str())
}

/// Datasets of `model` literally qualified in `expr` by a `<name>.` or
/// `"<name>".` prefix — the same textual detection `check_metrics` uses to
/// decide which bound datasets a metric's cross-product join must include,
/// reused by the context document (ADR 0018 §7) to decide which route(s) a
/// metric's "full" placement belongs under. Never a real SQL parse: a
/// metric that doesn't qualify any dataset (e.g. `COUNT(*)`) returns an
/// empty list, meaning "every bound dataset of the model", not "none" — see
/// both call sites for how they treat that case.
pub fn referenced_datasets<'a>(model: &'a SemanticModel, expr: &str) -> Vec<&'a Dataset> {
    model
        .datasets
        .iter()
        .filter(|ds| {
            expr.contains(&format!("{}.", ds.name)) || expr.contains(&format!("\"{}\".", ds.name))
        })
        .collect()
}

/// ADR 0018 §6, fields + `primary_key`: every dataset bound to `route`,
/// checked against `export`. `source` is the same string `verify::check`
/// already passed to `describe` for this export — the live view for a
/// bound export, the lake table for a materialized one.
///
/// Follow-up 8: a false claim about one field or one dataset's
/// `primary_key` no longer aborts the rest of this call — it's recorded
/// `verified: false, reason: "error"` (fields) or `primary_key: "error"`
/// (the dataset), its message pushed onto `errors`, and every remaining
/// field/dataset is still checked. `errors` non-empty is still a failing
/// `datamk verify`; the caller (`verify::check_semantic`) aggregates every
/// entry into one error after every dataset/relationship/metric has been
/// attempted, so a 15-minute bind pass's results are never thrown away by
/// the first bad claim. The H2 shape gate (`assert_scalar_or_aggregate_shape`)
/// is the one exception: a refused expression shape is a security refusal,
/// not a data-truth claim, and still aborts immediately via `Result::Err`.
pub fn check_export(
    conn: &Connection,
    export: &Export,
    route: &str,
    source: &str,
    index: &SemanticIndex,
    errors: &mut Vec<(String, String)>,
) -> Result<Vec<DatasetCheck>> {
    let mut out = Vec::new();
    for (model_name, ds) in index.datasets_for_route(route) {
        let mut fields = BTreeMap::new();
        let mut dialect_only = Vec::new();

        for field in &ds.fields {
            let Some(expr) = ansi_sql(&field.expression) else {
                fields.insert(
                    field.name.clone(),
                    FieldCheck {
                        verified: false,
                        reason: Some("dialect".to_string()),
                    },
                );
                dialect_only.push(field.name.clone());
                continue;
            };
            let expr = expr.trim();
            if let Some(ident) = bare_identifier(expr) {
                if !export.schema.keys().any(|c| c.eq_ignore_ascii_case(&ident)) {
                    let declared: Vec<&str> = export.schema.keys().map(String::as_str).collect();
                    errors.push((
                        format!("{model_name}/{}", ds.name),
                        format!(
                            "model '{model_name}' dataset '{}' field '{}': identity expression \
                             '{expr}' is not a declared column of export '{route}' — declared \
                             columns: {}",
                            ds.name,
                            field.name,
                            if declared.is_empty() {
                                "(none)".to_string()
                            } else {
                                declared.join(", ")
                            }
                        ),
                    ));
                    fields.insert(
                        field.name.clone(),
                        FieldCheck {
                            verified: false,
                            reason: Some("error".to_string()),
                        },
                    );
                    continue;
                }
            } else {
                if let Err(e) = assert_scalar_or_aggregate_shape(expr) {
                    bail!(
                        "model '{model_name}' dataset '{}' field '{}': `{expr}` — {e}",
                        ds.name,
                        field.name
                    );
                }
                let sql = format!("DESCRIBE SELECT {expr} FROM {source}");
                if let Err(e) = describe_plan(conn, &sql) {
                    if is_missing_function_error(&e.to_string()) {
                        tracing::warn!(
                            model = %model_name,
                            dataset = %ds.name,
                            field = %field.name,
                            expression = %expr,
                            error = %e,
                            "DuckDB has no such function — recorded unverified:function, not a \
                             hard error (a foreign-dialect scalar/aggregate/macro authored under \
                             ANSI_SQL, e.g. HLL_COUNT.MERGE under BigQuery; a bad column stays a \
                             hard error)"
                        );
                        fields.insert(
                            field.name.clone(),
                            FieldCheck {
                                verified: false,
                                reason: Some("function".to_string()),
                            },
                        );
                        continue;
                    }
                    errors.push((
                        format!("{model_name}/{}", ds.name),
                        format!(
                            "model '{model_name}' dataset '{}' field '{}': DuckDB rejected \
                             `{expr}` — {e}. {NOTHING_EXECUTED}",
                            ds.name, field.name
                        ),
                    ));
                    fields.insert(
                        field.name.clone(),
                        FieldCheck {
                            verified: false,
                            reason: Some("error".to_string()),
                        },
                    );
                    continue;
                }
            }
            fields.insert(
                field.name.clone(),
                FieldCheck {
                    verified: true,
                    reason: None,
                },
            );
        }
        if !dialect_only.is_empty() {
            tracing::warn!(
                model = %model_name,
                dataset = %ds.name,
                fields = %dialect_only.join(", "),
                "field(s) have no ANSI_SQL expression variant — recorded unverified (dialect); \
                 datamk cannot plan-check a foreign dialect"
            );
        }

        let primary_key = if ds.primary_key.is_empty() {
            "absent".to_string()
        } else if export.grain.is_empty() {
            "no_grain".to_string()
        } else {
            let mut pk: Vec<String> = ds
                .primary_key
                .iter()
                .map(|s| s.to_ascii_lowercase())
                .collect();
            let mut grain: Vec<String> = export
                .grain
                .iter()
                .map(|s| s.to_ascii_lowercase())
                .collect();
            pk.sort();
            grain.sort();
            if pk == grain {
                "matches".to_string()
            } else {
                errors.push((
                    format!("{model_name}/{}", ds.name),
                    format!(
                        "model '{model_name}' dataset '{}': primary_key {:?} does not match \
                         export '{route}'s grain {:?} — the grain is the contract \
                         (uniqueness-checked every run); primary_key must restate it exactly",
                        ds.name, ds.primary_key, export.grain
                    ),
                ));
                "error".to_string()
            }
        };

        out.push(DatasetCheck {
            model: model_name.to_string(),
            dataset: ds.name.clone(),
            route: Some(route.to_string()),
            primary_key,
            fields,
        });
    }
    Ok(out)
}

fn export_by_route<'a>(def: &'a CellDef, route: &str) -> Option<&'a Export> {
    def.interface
        .iter()
        .find(|e| e.route().ok().as_deref() == Some(route))
}

/// ADR 0018 §6, relationships: every relationship of `model`, once. A
/// relationship whose `from`/`to` dataset isn't bound to any route in this
/// cell is `"unbound"`, never an error — an Ossie relationship spanning a
/// dataset this cell doesn't export is exactly ADR 0018 §5's "the same
/// `osi/` serves every cell cut from one repo."
///
/// Follow-up 8: a bad `from_columns`/`to_columns` claim is recorded
/// `"error"` and pushed onto `errors` rather than aborting the call — every
/// remaining relationship is still checked (see `check_export`'s doc
/// comment for why).
pub fn check_relationships(
    model: &SemanticModel,
    index: &SemanticIndex,
    def: &CellDef,
    errors: &mut Vec<(String, String)>,
) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for rel in &model.relationships {
        let from_ds = model.datasets.iter().find(|d| d.name == rel.from);
        let to_ds = model.datasets.iter().find(|d| d.name == rel.to);
        let from_route = index.route_for(&model.name, &rel.from).first().cloned();
        let to_route = index.route_for(&model.name, &rel.to).first().cloned();
        let (Some(from_ds), Some(to_ds), Some(from_route), Some(to_route)) =
            (from_ds, to_ds, from_route, to_route)
        else {
            out.insert(rel.name.clone(), "unbound".to_string());
            continue;
        };
        if rel.from_columns.len() != rel.to_columns.len() {
            errors.push((
                format!("{}/{}", model.name, rel.name),
                format!(
                    "model '{}' relationship '{}': from_columns {:?} and to_columns {:?} have \
                     different lengths.",
                    model.name, rel.name, rel.from_columns, rel.to_columns
                ),
            ));
            out.insert(rel.name.clone(), "error".to_string());
            continue;
        }
        let from_export = export_by_route(def, &from_route);
        let to_export = export_by_route(def, &to_route);
        let mut bad = false;
        for (side, ds, cols, export) in [
            ("from", from_ds, &rel.from_columns, from_export),
            ("to", to_ds, &rel.to_columns, to_export),
        ] {
            for c in cols {
                let is_field = ds.fields.iter().any(|f| f.name.eq_ignore_ascii_case(c));
                let is_column =
                    export.is_some_and(|e| e.schema.keys().any(|k| k.eq_ignore_ascii_case(c)));
                if !is_field && !is_column {
                    errors.push((
                        format!("{}/{}", model.name, rel.name),
                        format!(
                            "model '{}' relationship '{}': {side}_columns names '{c}', which is \
                             neither a field of dataset '{}' nor a declared column of the export \
                             it's bound to.",
                            model.name, rel.name, ds.name
                        ),
                    ));
                    bad = true;
                }
            }
        }
        out.insert(
            rel.name.clone(),
            if bad {
                "error".to_string()
            } else {
                "verified".to_string()
            },
        );
    }
    Ok(out)
}

/// ADR 0018 §6, metrics: every metric of `model`, once — `DESCRIBE SELECT
/// <expr> FROM (SELECT * FROM <src>) AS "<dataset>", … ` over the datasets
/// `expr` textually qualifies (`referenced_datasets`), so a metric that
/// qualifies a column by a dataset alias resolves the same way it would in
/// Ossie (H3: never every bound dataset of the model — cross-joining
/// datasets the expression never names manufactures a spurious "ambiguous
/// column" the moment two of them happen to share a name). An expression
/// that qualifies no dataset (`COUNT(*)`) is checked against the model's
/// one bound dataset when there is exactly one; with more than one it's
/// genuinely unknown which dataset an unqualified column belongs to, so
/// it's recorded `unverified:ambiguous` rather than guessed — the same
/// outcome a DuckDB binder error mentioning "ambiguous" gets, never a hard
/// `run`-failing error. No join is synthesized (ADR 0018, "Refused") — the
/// cross product (when there is one) is there so identifiers resolve, not
/// so the query means anything if it were ever run, which it never is.
///
/// Follow-up 8: a metric DuckDB genuinely rejects is recorded `"error"` and
/// pushed onto `errors` rather than aborting the call — every remaining
/// metric of this model is still checked (the scenario a 46-dataset
/// SQLMesh estate hit: two bad metrics in one model, the second never even
/// attempted under the old bail-on-first behavior).
pub fn check_metrics(
    conn: &Connection,
    model: &SemanticModel,
    index: &SemanticIndex,
    def: &CellDef,
    errors: &mut Vec<(String, String)>,
) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for metric in &model.metrics {
        let Some(expr) = ansi_sql(&metric.expression) else {
            out.insert(metric.name.clone(), "unverified:dialect".to_string());
            continue;
        };
        let expr = expr.trim();
        if let Err(e) = assert_scalar_or_aggregate_shape(expr) {
            bail!(
                "model '{}' metric '{}': `{expr}` — {e}",
                model.name,
                metric.name
            );
        }

        let referenced: Vec<&Dataset> = referenced_datasets(model, expr);
        let all_referenced_bound = referenced
            .iter()
            .all(|ds| !index.route_for(&model.name, &ds.name).is_empty());
        if !all_referenced_bound {
            out.insert(metric.name.clone(), "unverified:unbound".to_string());
            continue;
        }

        // H3: FROM only the datasets `expr` actually qualifies. An
        // unqualified expression is checked against the model's single
        // bound dataset when there's exactly one; with more than one,
        // which dataset it belongs to is unknown — record, don't guess.
        let from_datasets: Vec<&Dataset> = if !referenced.is_empty() {
            referenced
        } else {
            let bound_datasets: Vec<&Dataset> = model
                .datasets
                .iter()
                .filter(|ds| !index.route_for(&model.name, &ds.name).is_empty())
                .collect();
            match bound_datasets.len() {
                0 => {
                    out.insert(metric.name.clone(), "unverified:unbound".to_string());
                    continue;
                }
                1 => bound_datasets,
                _ => {
                    tracing::warn!(
                        model = %model.name,
                        metric = %metric.name,
                        "metric expression qualifies no dataset by name and more than one \
                         dataset of the model is bound — recorded unverified:ambiguous, not \
                         plan-checked"
                    );
                    out.insert(metric.name.clone(), "unverified:ambiguous".to_string());
                    continue;
                }
            }
        };

        let mut froms = Vec::with_capacity(from_datasets.len());
        for ds in &from_datasets {
            let route = &index.route_for(&model.name, &ds.name)[0];
            let Some(export) = export_by_route(def, route) else {
                continue;
            };
            let src = export
                .bind
                .as_deref()
                .unwrap_or_else(|| export.source_object());
            froms.push(format!(
                "(SELECT * FROM {src}) AS {}",
                quote_ident(&ds.name)
            ));
        }
        let sql = format!("DESCRIBE SELECT {expr} FROM {}", froms.join(", "));
        match describe_plan(conn, &sql) {
            Ok(()) => {
                out.insert(metric.name.clone(), "verified".to_string());
            }
            Err(e) if e.to_string().to_ascii_lowercase().contains("ambiguous") => {
                tracing::warn!(
                    model = %model.name,
                    metric = %metric.name,
                    error = %e,
                    "DuckDB reported an ambiguous reference — recorded unverified:ambiguous, \
                     not a hard error"
                );
                out.insert(metric.name.clone(), "unverified:ambiguous".to_string());
            }
            Err(e) if is_missing_function_error(&e.to_string()) => {
                tracing::warn!(
                    model = %model.name,
                    metric = %metric.name,
                    error = %e,
                    "DuckDB has no such function — recorded unverified:function, not a hard \
                     error (a foreign-dialect scalar/aggregate/macro authored under ANSI_SQL, \
                     e.g. HLL_COUNT.MERGE under BigQuery)"
                );
                out.insert(metric.name.clone(), "unverified:function".to_string());
            }
            Err(e) => {
                errors.push((
                    format!("{}/{}", model.name, metric.name),
                    format!(
                        "model '{}' metric '{}': DuckDB rejected `{expr}` — {e}. \
                         {NOTHING_EXECUTED}",
                        model.name, metric.name
                    ),
                ));
                out.insert(metric.name.clone(), "error".to_string());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ColumnSpec;
    use crate::ossie::bind::SemanticIndex;
    use crate::ossie::record::{Resolved, SemanticModelRecord};
    use crate::ossie::source::SemanticModelSource;
    use crate::ossie::{DialectExpression, Document, Expression, Field, Metric, Relationship};
    use indexmap::IndexMap;

    fn conn_with(sql: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(sql).unwrap();
        conn
    }

    fn ansi(expr: &str) -> Expression {
        Expression {
            dialects: vec![DialectExpression {
                dialect: Dialect::AnsiSql,
                expression: expr.to_string(),
            }],
        }
    }

    fn field(name: &str, expr: &str) -> Field {
        Field {
            name: name.to_string(),
            expression: ansi(expr),
            dimension: None,
            label: None,
            description: None,
            datatype: None,
            ai_context: None,
            custom_extensions: vec![],
        }
    }

    fn dialect_only_field(name: &str) -> Field {
        Field {
            name: name.to_string(),
            expression: Expression {
                dialects: vec![DialectExpression {
                    dialect: Dialect::Snowflake,
                    expression: "whatever".to_string(),
                }],
            },
            dimension: None,
            label: None,
            description: None,
            datatype: None,
            ai_context: None,
            custom_extensions: vec![],
        }
    }

    fn dataset(name: &str, source: &str, pk: &[&str], fields: Vec<Field>) -> Dataset {
        Dataset {
            name: name.to_string(),
            source: source.to_string(),
            primary_key: pk.iter().map(|s| s.to_string()).collect(),
            unique_keys: vec![],
            description: None,
            ai_context: None,
            fields,
            custom_extensions: vec![],
        }
    }

    fn model(
        name: &str,
        datasets: Vec<Dataset>,
        relationships: Vec<Relationship>,
        metrics: Vec<Metric>,
    ) -> SemanticModel {
        SemanticModel {
            name: name.to_string(),
            description: None,
            ai_context: None,
            datasets,
            relationships,
            metrics,
            custom_extensions: vec![],
        }
    }

    fn relationship(
        name: &str,
        from: &str,
        to: &str,
        from_cols: &[&str],
        to_cols: &[&str],
    ) -> Relationship {
        Relationship {
            name: name.to_string(),
            from: from.to_string(),
            to: to.to_string(),
            from_columns: from_cols.iter().map(|s| s.to_string()).collect(),
            to_columns: to_cols.iter().map(|s| s.to_string()).collect(),
            ai_context: None,
            custom_extensions: vec![],
        }
    }

    fn metric(name: &str, expr: &str) -> Metric {
        Metric {
            name: name.to_string(),
            expression: ansi(expr),
            description: None,
            datatype: None,
            ai_context: None,
            custom_extensions: vec![],
        }
    }

    fn dialect_only_metric(name: &str) -> Metric {
        Metric {
            name: name.to_string(),
            expression: Expression {
                dialects: vec![DialectExpression {
                    dialect: Dialect::Snowflake,
                    expression: "whatever".to_string(),
                }],
            },
            description: None,
            datatype: None,
            ai_context: None,
            custom_extensions: vec![],
        }
    }

    fn export(name: &str, bind: &str, cols: &[&str], grain: &[&str]) -> Export {
        let mut e: Export =
            serde_yaml::from_str(&format!("name: {name}\nversion: 1.0.0\n")).unwrap();
        e.bind = Some(bind.to_string());
        e.grain = grain.iter().map(|s| s.to_string()).collect();
        e.schema = cols
            .iter()
            .map(|c| (c.to_string(), ColumnSpec::bare("integer")))
            .collect::<IndexMap<_, _>>();
        e
    }

    fn record_with(models: Vec<SemanticModel>) -> SemanticModelRecord {
        SemanticModelRecord {
            datamk_version: "0.0.0".to_string(),
            cell_yaml_digest: "d".to_string(),
            synced_at: "2026-09-10T00:00:00Z".to_string(),
            source: SemanticModelSource::Dir {
                dir: "osi".to_string(),
            },
            resolved: Resolved::Dir {
                dir: "/abs/osi".to_string(),
            },
            content_sha256: "abc".to_string(),
            files: vec!["m.yaml".to_string()],
            model_files: IndexMap::new(),
            document: Document {
                version: "0.1.1".to_string(),
                dialects: vec![],
                vendors: vec![],
                semantic_model: models,
            },
        }
    }

    // --- fields ------------------------------------------------------------

    #[test]
    fn identity_field_ok_is_verified() {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id;");
        let ds = dataset("dsa", "dsa", &[], vec![field("id", "id")]);
        let idx = SemanticIndex::build(
            record_with(vec![model("m", vec![ds], vec![], vec![])]),
            &[export("dsa", "a_tbl", &["id"], &[])],
        );
        let exp = export("dsa", "a_tbl", &["id"], &[]);
        let mut errors = Vec::new();
        let checks = check_export(&conn, &exp, "dsa@1", "a_tbl", &idx, &mut errors).unwrap();
        assert_eq!(checks.len(), 1);
        assert!(checks[0].fields["id"].verified);
    }

    #[test]
    fn identity_field_missing_from_declared_columns_is_recorded_error_not_a_bailed_out_call() {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id;");
        let ds = dataset("dsa", "dsa", &[], vec![field("nope", "nope")]);
        let idx = SemanticIndex::build(
            record_with(vec![model("m", vec![ds], vec![], vec![])]),
            &[export("dsa", "a_tbl", &["id"], &[])],
        );
        let exp = export("dsa", "a_tbl", &["id"], &[]);
        let mut errors = Vec::new();
        let checks = check_export(&conn, &exp, "dsa@1", "a_tbl", &idx, &mut errors).unwrap();
        let fc = &checks[0].fields["nope"];
        assert!(!fc.verified);
        assert_eq!(fc.reason.as_deref(), Some("error"));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].1.contains("not a declared column"), "{errors:?}");
        assert!(errors[0].1.contains("id"), "{errors:?}");
    }

    #[test]
    fn computed_expression_ok_is_verified() {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id;");
        let ds = dataset("dsa", "dsa", &[], vec![field("plus_one", "id + 1")]);
        let idx = SemanticIndex::build(
            record_with(vec![model("m", vec![ds], vec![], vec![])]),
            &[export("dsa", "a_tbl", &["id"], &[])],
        );
        let exp = export("dsa", "a_tbl", &["id"], &[]);
        let mut errors = Vec::new();
        let checks = check_export(&conn, &exp, "dsa@1", "a_tbl", &idx, &mut errors).unwrap();
        assert!(checks[0].fields["plus_one"].verified);
    }

    #[test]
    fn computed_expression_bad_names_duckdbs_message_and_nothing_executed() {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id;");
        let ds = dataset("dsa", "dsa", &[], vec![field("bogus", "id + doesnotexist")]);
        let idx = SemanticIndex::build(
            record_with(vec![model("m", vec![ds], vec![], vec![])]),
            &[export("dsa", "a_tbl", &["id"], &[])],
        );
        let exp = export("dsa", "a_tbl", &["id"], &[]);
        let mut errors = Vec::new();
        let checks = check_export(&conn, &exp, "dsa@1", "a_tbl", &idx, &mut errors).unwrap();
        assert!(!checks[0].fields["bogus"].verified);
        assert_eq!(checks[0].fields["bogus"].reason.as_deref(), Some("error"));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].1.contains("Nothing was executed"), "{errors:?}");
    }

    #[test]
    fn dialect_only_field_is_recorded_unverified_dialect_and_does_not_error() {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id;");
        let ds = dataset(
            "dsa",
            "dsa",
            &[],
            vec![dialect_only_field("snowflake_only")],
        );
        let idx = SemanticIndex::build(
            record_with(vec![model("m", vec![ds], vec![], vec![])]),
            &[export("dsa", "a_tbl", &["id"], &[])],
        );
        let exp = export("dsa", "a_tbl", &["id"], &[]);
        let mut errors = Vec::new();
        let checks = check_export(&conn, &exp, "dsa@1", "a_tbl", &idx, &mut errors).unwrap();
        let fc = &checks[0].fields["snowflake_only"];
        assert!(!fc.verified);
        assert_eq!(fc.reason.as_deref(), Some("dialect"));
    }

    #[test]
    fn computed_expression_calling_a_missing_function_is_recorded_unverified_function_not_a_hard_error(
    ) {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id;");
        let ds = dataset(
            "dsa",
            "dsa",
            &[],
            vec![field("hll", "TOTALLY_MADE_UP_AGG_FUNC(id)")],
        );
        let idx = SemanticIndex::build(
            record_with(vec![model("m", vec![ds], vec![], vec![])]),
            &[export("dsa", "a_tbl", &["id"], &[])],
        );
        let exp = export("dsa", "a_tbl", &["id"], &[]);
        let mut errors = Vec::new();
        let checks = check_export(&conn, &exp, "dsa@1", "a_tbl", &idx, &mut errors).unwrap();
        let fc = &checks[0].fields["hll"];
        assert!(!fc.verified);
        assert_eq!(fc.reason.as_deref(), Some("function"));
    }

    #[test]
    fn computed_expression_naming_a_missing_column_is_recorded_error_not_unverified_function() {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id;");
        let ds = dataset("dsa", "dsa", &[], vec![field("bogus", "id + doesnotexist")]);
        let idx = SemanticIndex::build(
            record_with(vec![model("m", vec![ds], vec![], vec![])]),
            &[export("dsa", "a_tbl", &["id"], &[])],
        );
        let exp = export("dsa", "a_tbl", &["id"], &[]);
        let mut errors = Vec::new();
        let checks = check_export(&conn, &exp, "dsa@1", "a_tbl", &idx, &mut errors).unwrap();
        assert_eq!(checks[0].fields["bogus"].reason.as_deref(), Some("error"));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].1.contains("Nothing was executed"), "{errors:?}");
    }

    // --- primary_key ---------------------------------------------------------

    #[test]
    fn primary_key_matching_grain_is_recorded_matches() {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id;");
        let ds = dataset("dsa", "dsa", &["id"], vec![field("id", "id")]);
        let idx = SemanticIndex::build(
            record_with(vec![model("m", vec![ds], vec![], vec![])]),
            &[export("dsa", "a_tbl", &["id"], &["id"])],
        );
        let exp = export("dsa", "a_tbl", &["id"], &["id"]);
        let mut errors = Vec::new();
        let checks = check_export(&conn, &exp, "dsa@1", "a_tbl", &idx, &mut errors).unwrap();
        assert_eq!(checks[0].primary_key, "matches");
    }

    #[test]
    fn primary_key_differing_from_grain_is_recorded_error_not_a_bailed_out_call() {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id, 2 AS other;");
        let ds = dataset("dsa", "dsa", &["other"], vec![field("id", "id")]);
        let idx = SemanticIndex::build(
            record_with(vec![model("m", vec![ds], vec![], vec![])]),
            &[export("dsa", "a_tbl", &["id", "other"], &["id"])],
        );
        let exp = export("dsa", "a_tbl", &["id", "other"], &["id"]);
        let mut errors = Vec::new();
        let checks = check_export(&conn, &exp, "dsa@1", "a_tbl", &idx, &mut errors).unwrap();
        assert_eq!(checks[0].primary_key, "error");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].1.contains("does not match export"), "{errors:?}");
        assert!(
            errors[0].1.contains("primary_key must restate it exactly"),
            "{errors:?}"
        );
    }

    #[test]
    fn primary_key_with_no_export_grain_is_recorded_no_grain() {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id;");
        let ds = dataset("dsa", "dsa", &["id"], vec![field("id", "id")]);
        let idx = SemanticIndex::build(
            record_with(vec![model("m", vec![ds], vec![], vec![])]),
            &[export("dsa", "a_tbl", &["id"], &[])],
        );
        let exp = export("dsa", "a_tbl", &["id"], &[]);
        let mut errors = Vec::new();
        let checks = check_export(&conn, &exp, "dsa@1", "a_tbl", &idx, &mut errors).unwrap();
        assert_eq!(checks[0].primary_key, "no_grain");
    }

    #[test]
    fn dataset_with_no_primary_key_is_recorded_absent() {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id;");
        let ds = dataset("dsa", "dsa", &[], vec![field("id", "id")]);
        let idx = SemanticIndex::build(
            record_with(vec![model("m", vec![ds], vec![], vec![])]),
            &[export("dsa", "a_tbl", &["id"], &["id"])],
        );
        let exp = export("dsa", "a_tbl", &["id"], &["id"]);
        let mut errors = Vec::new();
        let checks = check_export(&conn, &exp, "dsa@1", "a_tbl", &idx, &mut errors).unwrap();
        assert_eq!(checks[0].primary_key, "absent");
    }

    // --- relationships + metrics: two datasets, each bound to its own export -

    /// `dsa` and `dsb`, each bound by name to its own export, over real
    /// tables — the scaffold `check_relationships`/`check_metrics` share.
    fn two_dataset_model(
        relationships: Vec<Relationship>,
        metrics: Vec<Metric>,
    ) -> (Connection, CellDef, SemanticModel) {
        let conn = conn_with(
            "CREATE TABLE a_tbl AS SELECT 1 AS id, 10 AS val; \
             CREATE TABLE b_tbl AS SELECT 1 AS id, 20 AS other;",
        );
        let exp_a = export("dsa", "a_tbl", &["id", "val"], &["id"]);
        let exp_b = export("dsb", "b_tbl", &["id", "other"], &["id"]);
        let def: CellDef = serde_yaml::from_str("cell: t\n").unwrap();
        let mut def = def;
        def.interface = vec![exp_a, exp_b];
        let ds_a = dataset("dsa", "dsa", &["id"], vec![field("id", "id")]);
        let ds_b = dataset("dsb", "dsb", &["id"], vec![field("id", "id")]);
        let model = model("m", vec![ds_a, ds_b], relationships, metrics);
        (conn, def, model)
    }

    fn index_for(def: &CellDef, model: &SemanticModel) -> SemanticIndex {
        SemanticIndex::build(record_with(vec![model.clone()]), &def.interface)
    }

    #[test]
    fn relationship_with_both_sides_bound_and_valid_columns_is_verified() {
        let (_, def, model) = two_dataset_model(
            vec![relationship("r", "dsa", "dsb", &["id"], &["id"])],
            vec![],
        );
        let idx = index_for(&def, &model);
        let mut errors = Vec::new();
        let out = check_relationships(&model, &idx, &def, &mut errors).unwrap();
        assert_eq!(out["r"], "verified");
    }

    #[test]
    fn relationship_with_an_unbound_side_is_recorded_unbound() {
        let (_, def, mut model) = two_dataset_model(vec![], vec![]);
        // A relationship to a dataset this model declares but nothing binds.
        model.datasets.push(dataset("dsc", "dsc", &[], vec![]));
        model
            .relationships
            .push(relationship("r", "dsa", "dsc", &["id"], &["id"]));
        let idx = index_for(&def, &model);
        let mut errors = Vec::new();
        let out = check_relationships(&model, &idx, &def, &mut errors).unwrap();
        assert_eq!(out["r"], "unbound");
    }

    #[test]
    fn relationship_naming_a_nonexistent_column_is_recorded_error_not_a_bailed_out_call() {
        let (_, def, model) = two_dataset_model(
            vec![relationship("r", "dsa", "dsb", &["nope"], &["id"])],
            vec![],
        );
        let idx = index_for(&def, &model);
        let mut errors = Vec::new();
        let out = check_relationships(&model, &idx, &def, &mut errors).unwrap();
        assert_eq!(out["r"], "error");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].1.contains("nope"), "{errors:?}");
    }

    #[test]
    fn metric_verified_across_two_bound_datasets() {
        let (conn, def, model) = two_dataset_model(
            vec![],
            vec![metric("total", "SUM(dsa.val) + SUM(dsb.other)")],
        );
        let idx = index_for(&def, &model);
        let mut errors = Vec::new();
        let out = check_metrics(&conn, &model, &idx, &def, &mut errors).unwrap();
        assert_eq!(out["total"], "verified");
    }

    #[test]
    fn metric_referencing_an_unbound_dataset_is_unverified_unbound() {
        let (conn, def, mut model) = two_dataset_model(vec![], vec![]);
        model.datasets.push(dataset("dsc", "dsc", &[], vec![]));
        model.metrics.push(metric("total", "SUM(dsc.whatever)"));
        let idx = index_for(&def, &model);
        let mut errors = Vec::new();
        let out = check_metrics(&conn, &model, &idx, &def, &mut errors).unwrap();
        assert_eq!(out["total"], "unverified:unbound");
    }

    #[test]
    fn metric_with_no_ansi_sql_variant_is_unverified_dialect() {
        let (conn, def, mut model) = two_dataset_model(vec![], vec![]);
        model.metrics.push(dialect_only_metric("total"));
        let idx = index_for(&def, &model);
        let mut errors = Vec::new();
        let out = check_metrics(&conn, &model, &idx, &def, &mut errors).unwrap();
        assert_eq!(out["total"], "unverified:dialect");
    }

    #[test]
    fn metric_duckdb_error_is_recorded_error_not_a_bailed_out_call() {
        let (conn, def, mut model) = two_dataset_model(vec![], vec![]);
        model.metrics.push(metric("total", "SUM(dsa.nonexistent)"));
        let idx = index_for(&def, &model);
        let mut errors = Vec::new();
        let out = check_metrics(&conn, &model, &idx, &def, &mut errors).unwrap();
        assert_eq!(out["total"], "error");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].1.contains("Nothing was executed"), "{errors:?}");
    }

    /// Follow-up 8: two metrics that both genuinely fail in the same model
    /// are both attempted and both recorded — the old bail-on-first
    /// behavior stopped after the first, so the estate scenario this
    /// fixes never even saw the second metric's failure.
    #[test]
    fn two_failing_metrics_in_one_model_are_both_named_and_both_recorded() {
        let (conn, def, mut model) = two_dataset_model(vec![], vec![]);
        model.metrics.push(metric("first", "SUM(dsa.nonexistent)"));
        model.metrics.push(metric("second", "SUM(dsb.alsomissing)"));
        let idx = index_for(&def, &model);
        let mut errors = Vec::new();
        let out = check_metrics(&conn, &model, &idx, &def, &mut errors).unwrap();
        assert_eq!(out["first"], "error");
        assert_eq!(out["second"], "error");
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors.iter().any(|(label, _)| label == "m/first"),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|(label, _)| label == "m/second"),
            "{errors:?}"
        );
    }

    #[test]
    fn metric_calling_a_missing_function_is_unverified_function_not_a_hard_error() {
        let (conn, def, mut model) = two_dataset_model(vec![], vec![]);
        model.metrics.push(metric(
            "total",
            "TOTALLY_MADE_UP_AGG_FUNC(dsa.val) + SUM(dsb.other)",
        ));
        let idx = index_for(&def, &model);
        let mut errors = Vec::new();
        let out = check_metrics(&conn, &model, &idx, &def, &mut errors).unwrap();
        assert_eq!(out["total"], "unverified:function");
    }

    // --- H2: no subquery/table function reaches DESCRIBE -------------------

    #[test]
    fn duckdb_prepare_does_not_reject_multi_statement_text_the_semicolon_gate_is_load_bearing() {
        // H2 asked us to confirm duckdb-rs's own `prepare` already refuses
        // multi-statement text, on the theory that the `;` refusal in
        // `assert_scalar_or_aggregate_shape` would then be belt-and-braces.
        // It does not: `Connection::prepare` on multi-statement SQL
        // executes every statement but the last as a side effect of
        // preparing, and hands back a prepared statement for the last one
        // only — `describe_plan` here reports success (`Ok(())`) for
        // `DESCRIBE SELECT id FROM a_tbl; DROP TABLE a_tbl`, and the table
        // is gone. The `;` gate in `assert_scalar_or_aggregate_shape` is
        // therefore the *only* defense against this, not a redundant one —
        // it must run, and does, before any expression reaches
        // `describe_plan` in `check_export`/`check_metrics`.
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id;");
        let result = describe_plan(&conn, "DESCRIBE SELECT id FROM a_tbl; DROP TABLE a_tbl");
        assert!(result.is_ok(), "{result:?}");
        let still_there = conn
            .prepare("SELECT count(*) FROM a_tbl")
            .and_then(|mut s| s.query_row([], |r| r.get::<_, i64>(0)));
        assert!(
            still_there.is_err(),
            "expected the DROP TABLE after ';' to have run: {still_there:?}"
        );
    }

    #[test]
    fn field_expression_with_semicolon_is_refused_before_describe() {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id;");
        let ds = dataset(
            "dsa",
            "dsa",
            &[],
            vec![field("bad", "id; DROP TABLE a_tbl")],
        );
        let idx = SemanticIndex::build(
            record_with(vec![model("m", vec![ds], vec![], vec![])]),
            &[export("dsa", "a_tbl", &["id"], &[])],
        );
        let exp = export("dsa", "a_tbl", &["id"], &[]);
        let mut errors = Vec::new();
        let err = check_export(&conn, &exp, "dsa@1", "a_tbl", &idx, &mut errors)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("subqueries and table functions are refused"),
            "{err}"
        );
    }

    #[test]
    fn field_expression_with_a_subquery_keyword_is_refused() {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id;");
        let ds = dataset(
            "dsa",
            "dsa",
            &[],
            vec![field("bad", "(SELECT id FROM a_tbl WHERE id = 1)")],
        );
        let idx = SemanticIndex::build(
            record_with(vec![model("m", vec![ds], vec![], vec![])]),
            &[export("dsa", "a_tbl", &["id"], &[])],
        );
        let exp = export("dsa", "a_tbl", &["id"], &[]);
        let mut errors = Vec::new();
        let err = check_export(&conn, &exp, "dsa@1", "a_tbl", &idx, &mut errors)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("subqueries and table functions are refused"),
            "{err}"
        );
    }

    #[test]
    fn field_expression_calling_read_csv_is_refused() {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id;");
        let ds = dataset(
            "dsa",
            "dsa",
            &[],
            vec![field("bad", "(SELECT * FROM read_csv('/etc/passwd'))")],
        );
        let idx = SemanticIndex::build(
            record_with(vec![model("m", vec![ds], vec![], vec![])]),
            &[export("dsa", "a_tbl", &["id"], &[])],
        );
        let exp = export("dsa", "a_tbl", &["id"], &[]);
        let mut errors = Vec::new();
        let err = check_export(&conn, &exp, "dsa@1", "a_tbl", &idx, &mut errors)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("subqueries and table functions are refused"),
            "{err}"
        );
    }

    #[test]
    fn field_expression_calling_a_scan_suffixed_function_is_refused() {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id;");
        let ds = dataset(
            "dsa",
            "dsa",
            &[],
            vec![field(
                "bad",
                "(SELECT * FROM parquet_scan('s3://x/y.parquet'))",
            )],
        );
        let idx = SemanticIndex::build(
            record_with(vec![model("m", vec![ds], vec![], vec![])]),
            &[export("dsa", "a_tbl", &["id"], &[])],
        );
        let exp = export("dsa", "a_tbl", &["id"], &[]);
        let mut errors = Vec::new();
        let err = check_export(&conn, &exp, "dsa@1", "a_tbl", &idx, &mut errors)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("subqueries and table functions are refused"),
            "{err}"
        );
    }

    #[test]
    fn ordinary_computed_field_expression_is_unaffected_by_the_shape_gate() {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id, 2 AS selection_flag;");
        // `selection_flag` contains "select" as a substring but is not the
        // standalone token `SELECT` — must not be refused.
        let ds = dataset(
            "dsa",
            "dsa",
            &[],
            vec![field("plus_one", "id + selection_flag")],
        );
        let idx = SemanticIndex::build(
            record_with(vec![model("m", vec![ds], vec![], vec![])]),
            &[export("dsa", "a_tbl", &["id"], &[])],
        );
        let exp = export("dsa", "a_tbl", &["id"], &[]);
        let mut errors = Vec::new();
        let checks = check_export(&conn, &exp, "dsa@1", "a_tbl", &idx, &mut errors).unwrap();
        assert!(checks[0].fields["plus_one"].verified);
    }

    #[test]
    fn metric_expression_calling_read_parquet_is_refused() {
        let (conn, def, mut model) = two_dataset_model(vec![], vec![]);
        model.metrics.push(metric(
            "total",
            "(SELECT count(*) FROM read_parquet('s3://bucket/x.parquet'))",
        ));
        let idx = index_for(&def, &model);
        let mut errors = Vec::new();
        let err = check_metrics(&conn, &model, &idx, &def, &mut errors)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("subqueries and table functions are refused"),
            "{err}"
        );
    }

    // --- H3: unqualified/ambiguous metrics never hard-fail -----------------

    #[test]
    fn metric_qualifying_no_dataset_with_two_bound_datasets_is_unverified_ambiguous() {
        let (conn, def, mut model) = two_dataset_model(vec![], vec![]);
        // Neither `dsa.` nor `dsb.` appears — `referenced_datasets` returns
        // none, and more than one dataset is bound.
        model.metrics.push(metric("total", "COUNT(*)"));
        let idx = index_for(&def, &model);
        let mut errors = Vec::new();
        let out = check_metrics(&conn, &model, &idx, &def, &mut errors).unwrap();
        assert_eq!(out["total"], "unverified:ambiguous");
    }

    #[test]
    fn metric_qualifying_no_dataset_with_exactly_one_bound_dataset_is_verified() {
        let conn = conn_with("CREATE TABLE a_tbl AS SELECT 1 AS id, 10 AS val;");
        let exp_a = export("dsa", "a_tbl", &["id", "val"], &["id"]);
        let mut def: CellDef = serde_yaml::from_str("cell: t\n").unwrap();
        def.interface = vec![exp_a];
        let ds_a = dataset("dsa", "dsa", &["id"], vec![field("id", "id")]);
        let model = model("m", vec![ds_a], vec![], vec![metric("total", "SUM(val)")]);
        let idx = index_for(&def, &model);
        let mut errors = Vec::new();
        let out = check_metrics(&conn, &model, &idx, &def, &mut errors).unwrap();
        assert_eq!(out["total"], "verified");
    }

    #[test]
    fn metric_hitting_a_genuine_duckdb_ambiguous_column_is_unverified_ambiguous_not_a_hard_error() {
        let conn = conn_with(
            "CREATE TABLE a_tbl AS SELECT 1 AS id, 10 AS amount; \
             CREATE TABLE b_tbl AS SELECT 1 AS id, 20 AS amount;",
        );
        let exp_a = export("dsa", "a_tbl", &["id", "amount"], &["id"]);
        let exp_b = export("dsb", "b_tbl", &["id", "amount"], &["id"]);
        let mut def: CellDef = serde_yaml::from_str("cell: t\n").unwrap();
        def.interface = vec![exp_a, exp_b];
        let ds_a = dataset("dsa", "dsa", &["id"], vec![field("id", "id")]);
        let ds_b = dataset("dsb", "dsb", &["id"], vec![field("id", "id")]);
        // Both sides qualified (so `referenced_datasets` is non-empty and
        // both are FROM'd), but `amount` itself is left unqualified —
        // DuckDB's binder reports it ambiguous against the two-table FROM.
        let model = model(
            "m",
            vec![ds_a, ds_b],
            vec![],
            vec![metric("total", "SUM(dsa.id) + SUM(dsb.id) + SUM(amount)")],
        );
        let idx = index_for(&def, &model);
        let mut errors = Vec::new();
        let out = check_metrics(&conn, &model, &idx, &def, &mut errors).unwrap();
        assert_eq!(out["total"], "unverified:ambiguous");
    }
}
