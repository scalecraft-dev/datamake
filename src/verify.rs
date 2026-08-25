use anyhow::{bail, Context, Result};
use duckdb::Connection;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use crate::config::{CellDef, MaterializeStrategy, ResolvedTransform};
use crate::engine;

/// ADR 0008 decision 5: for an export over a keyed table, the grain IS the
/// key. One table has one uniqueness fact, and for `upsert`/`append` the
/// field that enforces it (`key:`) is the field that states it — declaring
/// both `key:` on the transform and `grain:` on the export would be double
/// bookkeeping. So an export whose `source` resolves to an `upsert`/`append`
/// table may omit `grain:` entirely (it inherits `key:`, as row identity and
/// the filterable query params both); an explicit `grain:` there is an
/// *extension* — additional filterable columns — and must contain every key
/// column (grain may be finer than key, never coarser: a grain missing a key
/// column aliases distinct keys and cannot be unique).
///
/// Called once, from `config::load` — the single seam every consumer of
/// `def.interface` (verify, serve, openapi) shares — so `export.grain` is
/// already the effective value everywhere it's read; no downstream consumer
/// needs to know strategies exist.  Pure and offline: no DB, checked at
/// resolve time, before any connection opens.
///
/// Applies only to the **key-bearing** strategies (`append`/`upsert`). A
/// `replace` table has no `key:` (nothing to reconcile against — ADR 0008
/// decision 3), so an export sourced from one has no key to inherit:
/// `grain:` stays exactly as declared, required and runtime-checked by
/// `check` below, never auto-populated — that remains grain's load-bearing
/// role for every table this ADR doesn't hand a key to.
pub(crate) fn apply_declarative_grain_inheritance(
    def: &mut CellDef,
    transforms: &[ResolvedTransform],
) -> Result<()> {
    // Exhaustive, not `!matches!(.., Replace)` (issue #6 merge-blocker): a
    // future strategy added to the enum without a matching arm here must
    // fail to compile, not silently fall into "contributes grain like
    // append/upsert" or "excluded like replace" by accident of the filter's
    // polarity. `Never` contributes no grain — nothing is stored, so there
    // is no key-bearing table to inherit from (same reason it's excluded
    // from the truncation gate below).
    let keys_by_table: HashMap<&str, &[String]> = transforms
        .iter()
        .filter_map(|t| match t.strategy {
            MaterializeStrategy::Append | MaterializeStrategy::Upsert => {
                Some((t.table.as_str(), t.key.as_slice()))
            }
            MaterializeStrategy::Replace | MaterializeStrategy::Never => None,
        })
        .collect();

    for export in &mut def.interface {
        if export.is_bound() {
            continue; // bound to a source, not a transform — no key to inherit.
        }
        let source = export.source_object().to_string();
        let Some(&key) = keys_by_table.get(source.as_str()) else {
            continue; // raw-sourced export — grain unchanged, still required.
        };

        if export.grain.is_empty() {
            export.grain = key.to_vec();
            continue;
        }

        let missing: Vec<&String> = key
            .iter()
            .filter(|k| !export.grain.iter().any(|g| g.eq_ignore_ascii_case(k)))
            .collect();
        if !missing.is_empty() {
            bail!(
                "export '{}': grain {:?} does not contain materialize key {:?} for table \
                 '{source}' (missing {missing:?}) — grain may be finer than the key (adding \
                 filterable columns) but must never be coarser: a grain missing a key column \
                 aliases distinct keys and cannot be unique.",
                export.name,
                export.grain,
                key,
            );
        }
    }
    Ok(())
}

/// Issue #6, binding model: `materialize: never` is a rejected legacy value
/// (see `MaterializeStrategy`'s doc comment) — the founder's decision: SQL
/// datamk itself never runs is a promise nothing keeps, so a virtual export
/// now binds directly to an existing object (`Export::bind`) instead.
/// Called from `config::mod::load`, right after `resolve_transforms`, with
/// the whole `CellDef` in view — so the error can name the affected
/// export(s), not just the offending transform file. Batches every offender
/// into ONE error (the first few, plus a count) rather than firing once per
/// transform and making the author re-run repeatedly to find the next one.
pub(crate) fn check_no_materialize_never(
    def: &CellDef,
    transforms: &[ResolvedTransform],
    dir: &Path,
) -> Result<()> {
    let offenders: Vec<&ResolvedTransform> = transforms
        .iter()
        .filter(|t| matches!(t.strategy, MaterializeStrategy::Never))
        .collect();
    if offenders.is_empty() {
        return Ok(());
    }

    const SHOWN: usize = 3;
    let lines: Vec<String> = offenders
        .iter()
        .take(SHOWN)
        .map(|t| describe_never_offender(def, t, dir))
        .collect();
    let more = offenders.len().saturating_sub(SHOWN);
    let mut tail = String::new();
    if more > 0 {
        tail.push_str(&format!("\n  ...and {more} more."));
    }

    bail!(
        "cell '{cell}' declares {n} `materialize: never` transform{s} — this strategy no \
         longer exists. `materialize: never` promised to serve rows datamk itself never \
         computed or stored: the SELECT ran nowhere except inside `verify`'s own session for a \
         few milliseconds, so a derived column, a rename, or a `WHERE` clause was a promise \
         nothing kept — on a PII surface, silent over-disclosure. A virtual export now binds \
         directly to an existing object instead (`bind:`, resolved through `sources:`/\
         `connections:` the same way every other source is) — there is no SQL to run at all.\n\
         \n\
         Fix each of the following:\n\
         {lines}{tail}\n\
         \n\
         For each, choose one:\n  \
         - `materialize: replace` (or `append`/`upsert`) on the transform, so datamk computes \
         and serves the rows itself; or\n  \
         - delete the transform and add `bind: <source>` to the export, pointing at an \
         existing object declared in `sources:` — push any derivation (renames, computed \
         columns, filters) upstream into that object.",
        cell = def.cell,
        n = offenders.len(),
        s = if offenders.len() == 1 { "" } else { "s" },
        lines = lines.join("\n"),
    );
}

/// One line naming the export(s) a rejected `never` transform backs, and —
/// best-effort — whether it looks convertible to a binding.
fn describe_never_offender(def: &CellDef, t: &ResolvedTransform, dir: &Path) -> String {
    let exports: Vec<&str> = def
        .interface
        .iter()
        .filter(|e| e.source_object() == t.table)
        .map(|e| e.name.as_str())
        .collect();
    let export_desc = if exports.is_empty() {
        "no export references it".to_string()
    } else {
        format!(
            "export{} {}",
            if exports.len() == 1 { "" } else { "s" },
            exports
                .iter()
                .map(|n| format!("'{n}'"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let hint = pure_passthrough_hint(def, t, dir)
        .map(|h| format!(": {h}"))
        .unwrap_or_default();
    format!("  - {export_desc} (transform '{}'){hint}", t.sql)
}

/// Best-effort migration hint: if a rejected `never` transform's SQL is
/// exactly `SELECT * FROM <source>` for a declared, bindable source, name it
/// — the customer's own `qfai_customer` shape. Never blocks and never
/// mis-fires into a wrong exit; it only ever *adds* a sentence. Anything
/// else (a rename, a derived column, a `WHERE`, a join, an unreadable file,
/// a `FROM` target that isn't a declared source) gets no hint, not a wrong
/// one — the two generic exits above still apply.
fn pure_passthrough_hint(def: &CellDef, t: &ResolvedTransform, dir: &Path) -> Option<String> {
    let content = std::fs::read_to_string(dir.join(&t.sql)).ok()?;
    let trimmed = content.trim().trim_end_matches(';').trim();
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.len() != 4
        || !tokens[0].eq_ignore_ascii_case("select")
        || tokens[1] != "*"
        || !tokens[2].eq_ignore_ascii_case("from")
    {
        return None;
    }
    let ident = tokens[3];
    let src = def.sources.get(ident)?;
    if !source_is_bindable(src) {
        return None;
    }
    Some(format!(
        "looks like a pure passthrough of source '{ident}' — add `bind: {ident}` to the export \
         and delete this transform"
    ))
}

/// Whether a declared source can back a binding (issue #6): a raw file/glob,
/// or a `connection:` source with `table:` — an existing, named object.
/// **Not** a `query:`-shaped connection source (ad hoc SQL nobody runs, the
/// exact failure the binding model replaces) and **not** (yet) a `cell:`
/// reference to another datamk cell — see `validate_bound_exports`, which
/// gives each excluded shape its own actionable error; this is the shared
/// predicate both it and the migration hint above use, so they can never
/// disagree on what counts as bindable.
fn source_is_bindable(src: &crate::config::Source) -> bool {
    matches!(src, crate::config::Source::Raw(_))
        || matches!(
            src,
            crate::config::Source::Connection {
                table: Some(_),
                query: None,
                ..
            }
        )
}

/// Issue #6, binding model: a bound export (`Export::bind`) must name a real
/// `sources:` entry of a bindable shape, and must not also set `source` (a
/// bound export has no transform table to override). Pure, offline — a
/// source's *shape* (raw/cell/connection, table vs query) is contract
/// (`cell.yaml`), not environment, so no `ResolvedBindings` is needed here.
pub(crate) fn validate_bound_exports(def: &CellDef) -> Result<()> {
    for export in &def.interface {
        let Some(bind) = &export.bind else {
            continue;
        };
        if export.source.is_some() {
            bail!(
                "export '{}': sets both `source` and `bind` — a bound export has no transform \
                 table to override. Remove `source` (or `bind`, if this export really reads a \
                 transform's table).",
                export.name
            );
        }
        check_bind_target(&export.name, bind, def)?;
    }
    Ok(())
}

/// The per-export half of `validate_bound_exports`'s shape check, factored
/// out so `datamk interface import` (issue #18) can run the exact same gate
/// against a prospective `--bind` name *before* an export exists to attach
/// it to — one source of truth for "can this source back a binding," never
/// two definitions that could drift.
pub(crate) fn check_bind_target(export_name: &str, bind: &str, def: &CellDef) -> Result<()> {
    let Some(src) = def.sources.get(bind) else {
        bail!(
            "export '{export_name}': `bind: {bind}` names no declared source — add `{bind}:` \
             under `sources:`, or point `bind:` at an existing one.",
        );
    };
    match src {
        crate::config::Source::Raw(_) => {}
        crate::config::Source::Connection {
            table: Some(_),
            query: None,
            ..
        } => {}
        crate::config::Source::Connection { query: Some(_), .. } => {
            bail!(
                "export '{export_name}': `bind: {bind}` names a `query:`-shaped connection \
                 source — that's ad hoc SQL nobody runs, the exact failure the binding model \
                 replaces, not an existing object to point at. Bind to a `table:`-shaped \
                 source instead (a real warehouse table or view), or materialize this export \
                 with a transform.",
            );
        }
        crate::config::Source::Connection {
            table: None,
            query: None,
            ..
        } => {
            // Unreachable in practice — `Source`'s own deserializer
            // already requires exactly one of `table`/`query`. Kept for
            // match exhaustiveness, not a real path.
            bail!(
                "export '{export_name}': `bind: {bind}` names a connection source with neither \
                 `table:` nor `query:` set.",
            );
        }
        crate::config::Source::Cell { .. } => {
            bail!(
                "export '{export_name}': `bind: {bind}` names a `cell:` source — binding to \
                 another cell's table isn't supported yet. Read it through a materializing \
                 transform instead.",
            );
        }
    }
    Ok(())
}

/// Verify a built cell against its declared interface (read-only).
///
/// A cell with any bound export (`Export::bind`, issue #6 binding model)
/// needs more than the plain read-only open above: standalone `verify`
/// binds no sources by default, so a bound export's session view (only ever
/// created by `datamk run`'s own unconditional `bind_sources`, or here)
/// does not exist yet. No dry-run of anything is needed — a bound export
/// has no transform; `engine::bind_sources` alone leaves a same-named
/// `TEMP VIEW` for every declared source, `check` below just describes it
/// directly. Skipped entirely for a cell with no bound exports — a
/// snapshot-only cell's verify pays no network round trip (and no billed
/// scan, ADR 0007 §4) it has never paid before.
///
/// On success, when a live check ran, `datamk context`'s
/// the document's `source_check` needs a persisted record — `verify` and
/// `context` are separate processes in CI, so the fact that a live check
/// passed does not otherwise survive past this process exiting. Written to
/// `.cell/source_check.json` (sibling of `.cell/published.json`, same
/// serialization style), stamped with the current `cell.yaml` digest so a
/// stale record (the config changed since this check ran) is detectable and
/// silently omitted rather than misapplied — see `context::emit`.
pub fn run(file: &Path, profile: &str) -> Result<()> {
    let cell = engine::open(file, profile, true)?;
    let has_bound_exports = cell.def.interface.iter().any(|e| e.is_bound());
    let warehouse_columns = if has_bound_exports {
        let (_, _, warehouse_columns) = engine::bind_sources(&cell, false)
            .context("binding sources for live verify of bound exports (issue #6)")?;
        warehouse_columns
    } else {
        HashMap::new()
    };
    let measurements = check(&cell.conn, &cell.def, &warehouse_columns)?;
    if has_bound_exports {
        write_source_check_record(file, &cell.dir, profile, measurements)
            .context("writing the live-verify source-check record (.cell/source_check.json)")?;
        // Issue #6/#10: the same live bind pass above already carries
        // `warehouse_columns` — persisted here under the identical
        // `has_bound_exports` gate (this is the only place standalone
        // verify binds sources at all, so it's the only place this fact is
        // ever available to write). A cell whose BigQuery sources feed only
        // materializing transforms never reaches this branch and so never
        // gets a `.cell/source_descriptions.json` — the same limitation
        // `.cell/source_check.json` already has, not a new one.
        let cell_yaml_digest = crate::context::cell_yaml_digest_of(file)?;
        crate::manifest::SourceDescriptionsRecord::write(
            &cell.dir,
            &cell_yaml_digest,
            profile,
            &cell.def,
            &warehouse_columns,
        )
        .context(
            "writing the live-verify source-descriptions record \
             (.cell/source_descriptions.json)",
        )?;
    }
    Ok(())
}

/// Persist the live-verify source-check record (issue #6): outcome, when,
/// which `datamk` built it, the `cell.yaml` digest at check time, and the
/// `profile` it ran under. The digest is the same one `datamk context`
/// computes for `cell_yaml_digest`, so `SourceCheckRecord::fresh_for` can
/// tell a fresh record from a stale one without re-deriving anything;
/// `profile` is the second, independent gate (issue #16) — a check under
/// `local` must not attest `prod`. `outcome` is `"passed"` by construction,
/// same discipline as `RunSummary.verify_outcome`: `check` above already
/// bailed on any failure, so this only ever runs after every check passed.
/// `data_as_of` stays `None` in this slice — no connector currently threads
/// a cheap, truthful "as of" timestamp out of the bind path; fabricating one
/// (or defaulting it to `checked_at`) is exactly what ADR 0012 §2 forbids.
fn write_source_check_record(
    file: &Path,
    dir: &Path,
    profile: &str,
    exports: BTreeMap<String, crate::manifest::GrainMeasurement>,
) -> Result<()> {
    let path = dir.join(".cell").join("source_check.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let cell_yaml_digest = crate::context::cell_yaml_digest_of(file)?;
    let record = crate::manifest::SourceCheckRecord {
        outcome: "passed".to_string(),
        checked_at: crate::timeutil::rfc3339_utc(crate::timeutil::unix_now()),
        data_as_of: None,
        datamk_version: env!("CARGO_PKG_VERSION").to_string(),
        cell_yaml_digest,
        profile: profile.to_string(),
        exports,
    };
    std::fs::write(&path, serde_json::to_string_pretty(&record)?)
        .with_context(|| format!("writing {}", path.display()))?;
    tracing::info!(path = %path.display(), "live-verify source-check recorded");
    Ok(())
}

/// The interface must not lie: every declared column must exist with a compatible
/// type, and every declared grain must exist and be unique in the actual output.
///
/// A bound export (issue #6, `Export::bind`) reads its declared source's
/// session view, not a transform table — that view is bound by
/// `engine::bind_sources`, unconditionally inside `datamk run`, and inside
/// standalone `datamk verify` by the live-verify bind pass at the top of
/// `run` above — so by the time `check` runs here, every bound export is
/// expected to already have a live view under its source's name. Describing
/// which name a bound export actually reads (rather than treating every
/// export uniformly via `source_object()`) is what makes a missing view's
/// error legible: a describe failure for a bound export names the binding
/// and where it was supposed to happen instead of leaving the caller to
/// guess at a raw "table does not exist."
///
/// `warehouse_columns` (issue #6/#9) is `engine::bind_sources`'s third
/// return value: per-source warehouse-native column types, keyed by source
/// name. Per-export dispatch, not per-cell — a materialized export always
/// checks against DuckDB's `DESCRIBE` of `lake` (there is no other
/// authority for a computed table); a bound export checks against its
/// source's warehouse-native types when they're available, and against
/// DuckDB's `DESCRIBE` of the bound view otherwise (a raw file, or a
/// connector with no classification job — not a lesser fallback, there is
/// genuinely no other authority to consult there either). Mixed cells hit
/// both paths in the same loop.
/// Returns what the grain check measured, per route key — the numbers behind
/// a passing check, for `.cell/source_check.json` and from there
/// `exports[].check`. A grainless export contributes nothing:
/// no check ran on it.
pub fn check(
    conn: &Connection,
    def: &CellDef,
    warehouse_columns: &HashMap<String, crate::engine::SourceWarehouseColumns>,
) -> Result<BTreeMap<String, crate::manifest::GrainMeasurement>> {
    // ADR 0005 §1: `__datamk_` is a reserved, enforced namespace — a table
    // matching it other than the watermark table itself is refused before
    // publish.
    check_reserved_prefix(conn)?;
    // ADR 0005 §2 item 3: the no-grain backstop warning, fired on every
    // run/verify. It only needs the raw (unresolved) definition — whether a
    // source is incremental and whether any export declares a grain are both
    // static, contract-only facts.
    warn_no_grain_backstop(def);
    // ADR 0012 §3 ratchet: the promotion gesture is where meaning becomes
    // mandatory — an experimental export needs nothing. Issue #18:
    // `warehouse_columns` lets a bound export satisfy this via meaning
    // already documented at the source, not only a locally authored one.
    check_supported_have_descriptions(def, warehouse_columns)?;

    let mut measurements = BTreeMap::new();
    for export in &def.interface {
        let source = export
            .bind
            .as_deref()
            .unwrap_or_else(|| export.source_object());
        let actual = match describe(conn, source) {
            Ok(actual) => actual,
            Err(e) if export.is_bound() => {
                return Err(e).with_context(|| {
                    format!(
                        "export '{}': `bind: {source}` — no live view named '{source}' in this \
                         session. Inside `datamk run`, `engine::bind_sources` binds every \
                         declared source unconditionally before transforms run; inside \
                         standalone `datamk verify`, the live-verify bind pass (issue #6) does \
                         — this failure means that pass didn't leave a '{source}' view behind \
                         (a missing/renamed `sources:` entry, or a connector error)",
                        export.name
                    )
                })
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("describing source '{source}' for export '{}'", export.name)
                })
            }
        };

        // Issue #6/#9: the warehouse-native columns for THIS export's bound
        // source, if any exist — looked up once per export, not per column.
        let warehouse = export
            .bind
            .as_ref()
            .and_then(|bind| warehouse_columns.get(bind));

        for (col, spec) in &export.schema {
            let declared_ty = &spec.ty;
            match actual.iter().find(|(c, _)| c.eq_ignore_ascii_case(col)) {
                None => bail!(
                    "export '{}': declared column '{col}' missing from source '{source}'",
                    export.name
                ),
                Some((_, actual_ty)) => {
                    let (ok, compared_ty) = column_type_ok(warehouse, col, declared_ty, actual_ty);
                    // ADR 0012 §7 (breaking change, promoted from a
                    // warning): a declared type asserted to a machine must
                    // be true — agents consume the interface as fact, so a
                    // lying type is a silent wrong number, not a nuisance
                    // log line.
                    if !ok {
                        bail!(
                            "export '{}': declared type '{declared_ty}' for column '{col}' does \
                             not match actual type '{compared_ty}' in source '{source}' — fix \
                             the declared schema or the transform so the interface tells the \
                             truth (previously a warning; promoted to an error by ADR 0012)",
                            export.name
                        );
                    }
                }
            }
        }

        for g in &export.grain {
            if !actual.iter().any(|(c, _)| c.eq_ignore_ascii_case(g)) {
                bail!(
                    "export '{}': grain column '{g}' missing from source '{source}'",
                    export.name
                );
            }
        }

        if !export.grain.is_empty() {
            let (total, distinct) = grain_counts(conn, source, &export.grain)?;
            if total != distinct {
                // ADR 0005 §2 item 5: in a cell with incremental sources, a
                // grain violation is most often a non-replay-safe transform
                // re-inserting a delta. Name that likely cause — the engine
                // cannot attribute the table to a source without parsing SQL.
                let hint = if has_incremental_source(def) {
                    " — if this table consumes an incremental source, the transform \
                     is likely not replay-safe (see docs/guides/incremental.md)"
                } else {
                    ""
                };
                bail!(
                    "export '{}': grain {:?} is not unique ({total} rows, {distinct} distinct){hint}",
                    export.name,
                    export.grain
                );
            }
            measurements.insert(
                export.route()?,
                crate::manifest::GrainMeasurement {
                    check: "grain_unique".to_string(),
                    grain: export.grain.clone(),
                    rows: total,
                    distinct_grain: distinct,
                },
            );
        }

        tracing::info!(export = %export.name, version = %export.version, "interface ok");
    }
    Ok(measurements)
}

/// ADR 0012 §3 ratchet check 4: an export with `contract: supported` must
/// have its meaning available to an agent — friction lands exactly on the
/// deliberate promotion gesture, and nowhere else. Pure contract fact,
/// checked on every run/verify; a supported export an agent cannot orient
/// on is a promotion that didn't finish. ADR 0013 extends the message
/// rather than adding a second check: a `docs:` page is additive prose,
/// never a substitute — an agent reads `description` before it ever
/// fetches a page, so `docs:` alone does not satisfy this lint. Setting
/// both is correct, not an error.
///
/// Issue #18: for a bound export, meaning satisfies this lint either way —
/// authored locally (`description:`) or already available live at the
/// source (the warehouse column descriptions `verify` records, i.e. the bound object has at
/// least one warehouse-documented column). Deliberately NOT "restated
/// locally" as the only path: `datamk interface import` exists precisely
/// because copying warehouse prose into `cell.yaml` is the rot ADR 0012 §3
/// already warns about, and forcing every `contract: supported` bound
/// export to carry a local description would make that copy mandatory —
/// the exact thing importing types-only is designed to avoid. An author
/// still writes `description:` when they mean something *different* from
/// the warehouse's own words; that's authorship, not a requirement this
/// lint imposes.
fn check_supported_have_descriptions(
    def: &CellDef,
    warehouse_columns: &HashMap<String, crate::engine::SourceWarehouseColumns>,
) -> Result<()> {
    for export in &def.interface {
        if export.contract != crate::config::Contract::Supported {
            continue;
        }
        let has_local_description = export
            .description
            .as_deref()
            .is_some_and(|d| !d.trim().is_empty());
        if has_local_description {
            continue;
        }
        let meaning_available_at_source = export
            .bind
            .as_deref()
            .and_then(|bind| warehouse_columns.get(bind))
            .is_some_and(|wc| !wc.descriptions.is_empty());
        if meaning_available_at_source {
            continue;
        }
        bail!(
            "export '{}': `contract: supported` requires a non-empty `description` — a \
             `docs:` page does not satisfy it, and neither does a bound source with no \
             warehouse-documented columns. One or two sentences: what one row means (ADR \
             0012 §3) — or, for a bound export, let the meaning already documented at the \
             source satisfy this (`datamk verify` records the warehouse's column descriptions). \
             Supported is the deliberate promotion gesture; a supported export without \
             meaning anywhere is a promotion that didn't finish. Agents read `description` \
             before they fetch a page.",
            export.name
        );
    }
    Ok(())
}

/// R8: `\_\_datamk\_%` with `ESCAPE '\'` — a bare `_` is a LIKE wildcard, so
/// the naive (unescaped) pattern over-matches any two-char-then-anything
/// table name (`ab_datamk_x`). Every table matching the *escaped* pattern
/// other than `__datamk_watermarks` itself is a contract violation: the
/// prefix is engine-owned and reserved for bookkeeping, not advisory.
fn check_reserved_prefix(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_catalog = 'lake' AND table_schema = 'main' \
         AND table_name LIKE '\\_\\_datamk\\_%' ESCAPE '\\' \
         ORDER BY table_name",
    )?;
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<_, _>>()?;
    for n in names {
        if n != "__datamk_watermarks" {
            bail!(
                "verify: table '{n}' uses the reserved `__datamk_` prefix, which is \
                 engine-owned (watermarks and future bookkeeping). Rename it — only \
                 `__datamk_watermarks` may use this prefix."
            );
        }
    }
    Ok(())
}

/// Names of every source in `def` declaring `incremental:` — a static,
/// contract-only fact read from the raw definition. Shared by the no-grain
/// backstop (`sources_without_grain_backstop`, ADR 0005 §2 item 3) and the
/// `materialize: replace` incremental-cell gate (`check_replace_incremental_
/// gate`, ADR 0008 §3) — one predicate, two consumers, so they can never
/// silently drift apart on what counts as "this cell has an incremental
/// source."
fn incremental_source_names(def: &CellDef) -> Vec<&str> {
    def.sources
        .iter()
        .filter_map(|(name, src)| {
            matches!(
                src,
                crate::config::Source::Connection {
                    incremental: Some(_),
                    ..
                }
            )
            .then_some(name.as_str())
        })
        .collect()
}

/// Whether any source in the cell declares `incremental:`.
fn has_incremental_source(def: &CellDef) -> bool {
    !incremental_source_names(def).is_empty()
}

/// ADR 0008 guard 4c, resolve-time hard error: a `replace` model that
/// references an incremental source's name. `replace` rebuilds a table from
/// scratch every run with no reconciliation against prior state, so it is
/// replay-safe only when its SELECT reads a complete relation, never a
/// partial delta — and reading an incremental source directly is reading a
/// delta by definition. The engine cannot see *which* relations a model's
/// SELECT reads (decision 1: never parse the SELECT), so it can't literally
/// verify "this SELECT is safe" — but it can scan the model's file text for
/// the exact, engine-owned names that *would* make it unsafe if referenced,
/// which is metadata matching, not query comprehension. Per model, not
/// per cell (the ban this replaced): a `replace` rollup over an
/// `upsert`/`append` accumulator *in the same incremental cell* is fine —
/// it reads a complete table, not the delta — and the old cell-wide ban
/// wrongly forbade exactly that shape.
///
/// The scan is a **word-boundary token match** against source names the
/// engine owns (`sources:` keys), not SQL parsing: `events` matches `FROM
/// events e` but not `FROM fct_events` (a real accumulator table, not the
/// source) — the boundary characters on both sides of a match must be
/// non-identifier (`[^A-Za-z0-9_]` or start/end of file). Two honest edges,
/// named rather than hidden: a source name appearing in a `--` comment
/// false-positives (the fix is to remove or reword the comment — no
/// behavior changes, only text the scanner reads); indirection that evades
/// the literal token (a CTE alias, a view, string-built SQL) evades the
/// scan too and falls through to the shrink detector instead, same backstop
/// role it already plays for every other truncation risk.
///
/// **Coupling, on the record (ADR 0008 guard 4c, required at the guard
/// site):** this predicate is sound only while incremental `connection`
/// sources are the *sole* delta-producers this engine has. ADR 0005's
/// deferred incremental `Cell`/`Raw` sources do not exist yet; the day
/// either ships, `incremental_source_names` (and therefore the set of names
/// this scan matches against) MUST extend to cover them too, or the
/// founding incident reopens through exactly this gate's blind spot — a
/// `replace` model reading a not-yet-recognized incremental delta,
/// unscanned because this predicate only ever looked at `connection`
/// sources.
pub(crate) fn check_replace_incremental_gate(
    def: &CellDef,
    dir: &Path,
    transforms: &[ResolvedTransform],
) -> Result<()> {
    let incremental_sources = incremental_source_names(def);
    if incremental_sources.is_empty() {
        return Ok(());
    }
    for t in transforms {
        // Exhaustive, not `!matches!(.., Replace)` (issue #6 merge-blocker):
        // a future strategy added without a matching arm here must fail to
        // compile, not silently join `replace` in this scan (which would
        // misfire — the scan's whole premise is "rebuilds from scratch",
        // untrue for append/upsert/never) or silently skip it (which would
        // reopen guard 4c's blind spot). `never` writes nothing to the lake
        // at all, so there is no truncation risk here to catch, same reason
        // it contributes no grain above.
        match t.strategy {
            MaterializeStrategy::Replace => {}
            MaterializeStrategy::Append
            | MaterializeStrategy::Upsert
            | MaterializeStrategy::Never => continue,
        }
        let sql_path = dir.join(&t.sql);
        let text = std::fs::read_to_string(&sql_path)
            .with_context(|| format!("reading transform {}", sql_path.display()))?;
        for &source in &incremental_sources {
            if contains_word_token(&text, source) {
                bail!(
                    "transform '{}': materialize: replace references incremental source \
                     '{source}' — rebuilding from the delta would replace the table's history \
                     with just the delta (truncation). Read the accumulated table instead (an \
                     upsert/append model over '{source}' in this cell), or change this model to \
                     materialize: upsert/append if it should itself accumulate. See \
                     docs/guides/incremental.md §4.",
                    t.sql
                );
            }
        }
    }
    Ok(())
}

/// Whether `text` contains `word` as a whole token — bounded on both sides
/// by a non-identifier character (`[^A-Za-z0-9_]`) or the start/end of the
/// text. A pure string scan (ADR 0008 guard 4c: "not SQL parsing"), so it
/// makes no attempt to distinguish code from comments or string literals —
/// that imprecision is deliberate and documented at the call site.
fn contains_word_token(text: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut search_from = 0;
    while let Some(offset) = text[search_from..].find(word) {
        let start = search_from + offset;
        let end = start + word.len();
        let before_ok = text[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident(c));
        let after_ok = text[end..].chars().next().is_none_or(|c| !is_ident(c));
        if before_ok && after_ok {
            return true;
        }
        search_from = start + 1;
        if search_from >= text.len() {
            break;
        }
    }
    false
}

/// The incremental sources with no grain backstop (ADR 0005 §2 item 3): no
/// export anywhere in the interface declares a non-empty grain, so
/// `verify`'s grain-uniqueness check cannot catch a duplicating transform
/// for any of them. Pure — split out from `warn_no_grain_backstop` (the
/// thin logging wrapper) so this is unit-testable without a `tracing`
/// capture layer, matching this codebase's usual pure-computation/thin-I/O
/// split (`shrunk_tables`/`format_shrink_summary_lines`, `ops::
/// build_rollback_changes`/`format_rollback_lines`).
///
/// ADR 0008 decision 5 (grain): this is also the fallback a table whose
/// `materialize:` entry was removed from `transforms:` lands on. Grain
/// inheritance ends the moment that happens (`apply_declarative_grain_
/// inheritance` no longer sees a `materialize:` entry for that table), so
/// an export that relied on it and never restated its own `grain:` has
/// none — and trips this check, as long as the cell's source is a
/// detectable `incremental:` connection (the one accumulation shape this
/// check can see without parsing SQL; a purely local/file-sourced cell —
/// e.g. this repo's own `init` scaffold — gets no backstop here either way,
/// a pre-existing ADR 0005 limit this ADR does not attempt to close).
fn sources_without_grain_backstop(def: &CellDef) -> Vec<&str> {
    let has_any_grain = def.interface.iter().any(|e| !e.grain.is_empty());
    if has_any_grain {
        return Vec::new();
    }
    incremental_source_names(def)
}

fn warn_no_grain_backstop(def: &CellDef) {
    for name in sources_without_grain_backstop(def) {
        tracing::warn!(
            "incremental source '{name}' has no grain backstop: no export declares a grain, \
             so `verify` cannot catch a transform that duplicates this delta. Declare \
             `grain:` on the export, or gate CI with --verify-replay."
        );
    }
}

/// `pub(crate)`: `datamk interface import` (issue #18) reuses this exact
/// DESCRIBE, the same live-bound-session-view read `check` above uses — one
/// implementation of "what does this source's schema actually look like
/// right now," never a second copy that could drift.
pub(crate) fn describe(conn: &Connection, source: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(&format!("DESCRIBE SELECT * FROM {source}"))?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn grain_counts(conn: &Connection, source: &str, grain: &[String]) -> Result<(i64, i64)> {
    let cols = grain.join(", ");
    let mut stmt = conn.prepare(&format!(
        "SELECT (SELECT count(*) FROM {source}) AS total,
                (SELECT count(*) FROM (SELECT DISTINCT {cols} FROM {source})) AS distinct_grain"
    ))?;
    let row = stmt.query_row([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
    Ok(row)
}

/// Whether a declared column's type matches its actual type, and which type
/// string was actually compared against (for the error message — naming
/// DuckDB's `actual_ty` when a warehouse-native `NUMERIC` genuinely failed
/// would blame the wrong authority). Issue #6/#9 dispatch, per column: a
/// bound export whose source has a warehouse-native type for this column
/// (BigQuery only, today — `native_type_compatible` returns `None` for a
/// connector with no vocabulary) is checked against THAT authority; every
/// other column — materialized, raw-bound, or a connector with no
/// classification job — keeps the existing DuckDB-`DESCRIBE`-based
/// `type_compatible`, called unmodified below.
fn column_type_ok<'a>(
    warehouse: Option<&'a crate::engine::SourceWarehouseColumns>,
    col: &str,
    declared_ty: &str,
    actual_ty: &'a str,
) -> (bool, &'a str) {
    if let Some(src) = warehouse {
        if let Some((_, native_ty)) = src
            .columns
            .iter()
            .find(|(c, _)| c.eq_ignore_ascii_case(col))
        {
            if let Some(ok) = crate::engine::connectors::native_type_compatible(
                src.connector,
                declared_ty,
                native_ty,
            ) {
                return (ok, native_ty.as_str());
            }
        }
    }
    (type_compatible(declared_ty, actual_ty), actual_ty)
}

/// Loose structural compatibility between a declared type name and DuckDB's reported type.
fn type_compatible(declared: &str, actual: &str) -> bool {
    let a = actual.to_uppercase();
    match declared.to_lowercase().as_str() {
        "string" | "varchar" | "text" => a.starts_with("VARCHAR") || a == "TEXT",
        "int" | "integer" => a == "INTEGER" || a == "INT" || a == "INT32",
        "bigint" | "long" => a == "BIGINT" || a == "INT64",
        "decimal" | "numeric" => a.starts_with("DECIMAL") || a.starts_with("NUMERIC"),
        "double" | "float" => a == "DOUBLE" || a == "FLOAT" || a == "REAL",
        "bool" | "boolean" => a == "BOOLEAN",
        "date" => a == "DATE",
        "timestamp" => a.starts_with("TIMESTAMP"),
        other => a.to_lowercase() == other,
    }
}

/// `datamk interface import`'s (issue #18) DuckDB-facing type authority —
/// the inverse of `type_compatible` immediately above, which this never
/// modifies (a sibling, not an edit). This is the authority for every
/// column with no warehouse-native type to consult: a raw file, or a
/// connector with no metadata job (Postgres, Snowflake — ADR 0010), same
/// "not a lesser fallback, there is genuinely no other authority" framing
/// `column_type_ok` already uses for the check direction. One canonical
/// declared name per DuckDB type, chosen so `type_compatible(declared,
/// actual)` is true by construction for every pair this can produce —
/// pinned by a round-trip test, same discipline as BigQuery's
/// `declared_type_for`. `None` for a DuckDB type with no clean declared
/// name (`STRUCT(...)`, `LIST(...)`, `BLOB`, `TIME`, `UUID`, …) — the
/// caller emits `type: unmapped` naming the real type, never a guess.
pub(crate) fn duckdb_declared_type_for(actual: &str) -> Option<&'static str> {
    let a = actual.to_uppercase();
    if a.starts_with("VARCHAR") || a == "TEXT" {
        return Some("string");
    }
    if a == "INTEGER" || a == "INT" || a == "INT32" {
        return Some("integer");
    }
    if a == "BIGINT" || a == "INT64" {
        return Some("bigint");
    }
    if a.starts_with("DECIMAL") || a.starts_with("NUMERIC") {
        return Some("decimal");
    }
    if a == "DOUBLE" || a == "FLOAT" || a == "REAL" {
        return Some("double");
    }
    if a == "BOOLEAN" {
        return Some("boolean");
    }
    if a == "DATE" {
        return Some("date");
    }
    if a.starts_with("TIMESTAMP") {
        return Some("timestamp");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // A locally ATTACHed DuckLake, mirroring engine::mod's probe helpers —
    // this file has no engine test infra of its own, so it stands up just
    // enough of a `lake` catalog to exercise `check_reserved_prefix` for
    // real, including DuckDB's own LIKE/ESCAPE semantics.
    fn attach_lake(tag: &str) -> (Connection, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "datamk-verify-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open_in_memory().expect("open in-memory duckdb");
        conn.execute_batch("INSTALL ducklake; LOAD ducklake; INSTALL json; LOAD json;")
            .expect("install/load ducklake");
        let catalog = dir.join("verify_test.ducklake");
        let data = dir.join("data");
        conn.execute_batch(&format!(
            "ATTACH 'ducklake:{}' AS lake (DATA_PATH '{}'); USE lake;",
            catalog.to_string_lossy(),
            data.to_string_lossy()
        ))
        .expect("attach ducklake");
        (conn, dir)
    }

    // --- issue #6, binding model: the migration error -----------------------
    //
    // The customer-facing surface of the founder's decision (`materialize:
    // never` is gone) — the shape that matters per the coordinator's brief:
    // name the export and the offending transform, say why in one clause,
    // give both exits with `materialize:` first, batch multiple offenders.

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datamk-verify-migration-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sql")).unwrap();
        dir
    }

    fn resolved(def: &CellDef) -> Vec<ResolvedTransform> {
        crate::config::resolve_transforms(&def.transforms).unwrap()
    }

    #[test]
    fn check_no_materialize_never_passes_when_none_exist() {
        let dir = tempdir("clean");
        let def: CellDef = serde_yaml::from_str(
            "cell: t\ntransforms:\n  - sql/a.sql\ninterface:\n  - name: a\n    version: 1.0.0\n",
        )
        .unwrap();
        check_no_materialize_never(&def, &resolved(&def), &dir).expect("no never transforms");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The customer's `qfai_kpis_monthly` shape: derives columns, so no
    /// automatic hint — both exits are given, `materialize:` first (the one
    /// that needs no coordination with another team), naming the export.
    #[test]
    fn check_no_materialize_never_names_the_export_and_offers_both_exits_materialize_first() {
        let dir = tempdir("derives");
        std::fs::write(
            dir.join("sql/kpis.sql"),
            "SELECT id, revenue * 1.1 AS revenue_usd FROM raw",
        )
        .unwrap();
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             transforms:\n\
             \x20 - sql: sql/kpis.sql\n\
             \x20   materialize: never\n\
             interface:\n\
             \x20 - name: kpis\n\
             \x20   version: 1.0.0\n\
             sources:\n\
             \x20 raw: ./raw.csv\n",
        )
        .unwrap();
        let err = check_no_materialize_never(&def, &resolved(&def), &dir)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cell 't'"), "got: {err}");
        assert!(err.contains("no longer exists"), "got: {err}");
        assert!(
            err.contains("rows datamk itself never computed or stored"),
            "must say why in one clause: got: {err}"
        );
        assert!(
            err.contains("export 'kpis'"),
            "must name the export: got: {err}"
        );
        assert!(
            err.contains("sql/kpis.sql"),
            "must name the transform: got: {err}"
        );
        assert!(
            !err.contains("looks like a pure passthrough"),
            "a column-deriving transform must get no convertibility hint: got: {err}"
        );
        let materialize_at = err
            .find("`materialize: replace`")
            .expect("materialize exit present");
        let bind_at = err.find("add `bind:").expect("bind exit present");
        assert!(
            materialize_at < bind_at,
            "materialize: must be listed first (needs no coordination with another team): \
             got: {err}"
        );
    }

    /// The customer's `qfai_customer` shape: a pure passthrough of a
    /// declared, bindable source — gets the convertibility hint by name.
    #[test]
    fn check_no_materialize_never_hints_a_pure_passthrough_as_bindable() {
        let dir = tempdir("passthrough");
        std::fs::write(dir.join("sql/customer.sql"), "SELECT * FROM raw").unwrap();
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             transforms:\n\
             \x20 - sql: sql/customer.sql\n\
             \x20   materialize: never\n\
             interface:\n\
             \x20 - name: customer\n\
             \x20   version: 1.0.0\n\
             sources:\n\
             \x20 raw: ./raw.csv\n",
        )
        .unwrap();
        let err = check_no_materialize_never(&def, &resolved(&def), &dir)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("looks like a pure passthrough of source 'raw'"),
            "got: {err}"
        );
        assert!(err.contains("add `bind: raw`"), "got: {err}");
    }

    /// The hint is best-effort and must never mis-fire: a `FROM` target that
    /// isn't a declared, bindable source gets no hint, not a wrong one.
    #[test]
    fn check_no_materialize_never_gives_no_hint_when_the_from_target_is_not_a_bindable_source() {
        let dir = tempdir("not-bindable");
        std::fs::write(dir.join("sql/x.sql"), "SELECT * FROM some_other_table").unwrap();
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             transforms:\n\
             \x20 - sql: sql/x.sql\n\
             \x20   materialize: never\n\
             interface:\n\
             \x20 - name: x\n\
             \x20   version: 1.0.0\n",
        )
        .unwrap();
        let err = check_no_materialize_never(&def, &resolved(&def), &dir)
            .unwrap_err()
            .to_string();
        assert!(!err.contains("looks like a pure passthrough"), "got: {err}");
    }

    #[test]
    fn check_no_materialize_never_names_no_export_for_an_orphaned_transform() {
        let dir = tempdir("orphan");
        std::fs::write(dir.join("sql/orphan.sql"), "SELECT 1 AS x").unwrap();
        let def: CellDef =
            serde_yaml::from_str("cell: t\ntransforms:\n  - sql: sql/orphan.sql\n    materialize: never\ninterface: []\n")
                .unwrap();
        let err = check_no_materialize_never(&def, &resolved(&def), &dir)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no export references it"), "got: {err}");
    }

    #[test]
    fn check_no_materialize_never_batches_multiple_offenders_naming_a_few_plus_a_count() {
        let dir = tempdir("batch");
        let mut yaml = "cell: t\ntransforms:\n".to_string();
        let mut interface = "interface:\n".to_string();
        for i in 0..5 {
            yaml.push_str(&format!("  - sql: sql/t{i}.sql\n    materialize: never\n"));
            interface.push_str(&format!("  - name: t{i}\n    version: 1.0.0\n"));
            std::fs::write(dir.join(format!("sql/t{i}.sql")), "SELECT 1 AS x").unwrap();
        }
        yaml.push_str(&interface);
        let def: CellDef = serde_yaml::from_str(&yaml).unwrap();
        let err = check_no_materialize_never(&def, &resolved(&def), &dir)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("declares 5 `materialize: never` transforms"),
            "got: {err}"
        );
        assert!(err.contains("export 't0'"), "got: {err}");
        assert!(err.contains("export 't1'"), "got: {err}");
        assert!(err.contains("export 't2'"), "got: {err}");
        assert!(
            !err.contains("export 't3'"),
            "must not list every offender, only the first few: got: {err}"
        );
        assert!(err.contains("...and 2 more"), "got: {err}");
    }

    // --- issue #6, binding model: `validate_bound_exports` ------------------

    #[test]
    fn validate_bound_exports_passes_for_a_raw_bound_export() {
        let def: CellDef = serde_yaml::from_str(
            "cell: t\ninterface:\n  - name: a\n    version: 1.0.0\n    bind: raw\nsources:\n  raw: ./raw.csv\n",
        )
        .unwrap();
        validate_bound_exports(&def).expect("a raw source can back a binding");
    }

    #[test]
    fn validate_bound_exports_passes_for_a_table_connection_bound_export() {
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             interface:\n\
             \x20 - name: a\n    version: 1.0.0\n    bind: raw\n\
             sources:\n\
             \x20 raw:\n    connection: bq\n    table: dataset.table\n",
        )
        .unwrap();
        validate_bound_exports(&def).expect("a table-shaped connection source can back a binding");
    }

    #[test]
    fn validate_bound_exports_rejects_both_source_and_bind_set() {
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             interface:\n\
             \x20 - name: a\n    version: 1.0.0\n    source: something\n    bind: raw\n\
             sources:\n\
             \x20 raw: ./raw.csv\n",
        )
        .unwrap();
        let err = validate_bound_exports(&def).unwrap_err().to_string();
        assert!(err.contains("export 'a'"), "got: {err}");
        assert!(err.contains("both `source` and `bind`"), "got: {err}");
    }

    #[test]
    fn validate_bound_exports_rejects_an_undeclared_source_name() {
        let def: CellDef = serde_yaml::from_str(
            "cell: t\ninterface:\n  - name: a\n    version: 1.0.0\n    bind: missing\n",
        )
        .unwrap();
        let err = validate_bound_exports(&def).unwrap_err().to_string();
        assert!(err.contains("export 'a'"), "got: {err}");
        assert!(err.contains("names no declared source"), "got: {err}");
    }

    #[test]
    fn validate_bound_exports_rejects_a_query_shaped_connection_source() {
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             interface:\n\
             \x20 - name: a\n    version: 1.0.0\n    bind: raw\n\
             sources:\n\
             \x20 raw:\n    connection: bq\n    query: SELECT 1\n",
        )
        .unwrap();
        let err = validate_bound_exports(&def).unwrap_err().to_string();
        assert!(err.contains("export 'a'"), "got: {err}");
        assert!(err.contains("ad hoc SQL nobody runs"), "got: {err}");
    }

    #[test]
    fn validate_bound_exports_rejects_a_cell_shaped_source() {
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             interface:\n\
             \x20 - name: a\n    version: 1.0.0\n    bind: upstream\n\
             sources:\n\
             \x20 upstream:\n    cell: other\n    table: fct\n",
        )
        .unwrap();
        let err = validate_bound_exports(&def).unwrap_err().to_string();
        assert!(err.contains("export 'a'"), "got: {err}");
        assert!(
            err.contains("binding to another cell's table isn't supported yet"),
            "got: {err}"
        );
    }

    // R8: `_` is a LIKE wildcard — the escaped pattern must not over-match a
    // table whose name merely contains "datamk" two characters in.
    #[test]
    fn reserved_prefix_check_does_not_flag_a_two_char_prefix_match() {
        let (conn, _dir) = attach_lake("prefix-ok");
        conn.execute_batch("CREATE TABLE ab_datamk_x (id INTEGER);")
            .unwrap();
        check_reserved_prefix(&conn).expect("ab_datamk_x must not be flagged");
    }

    #[test]
    fn reserved_prefix_check_flags_an_engine_owned_prefix_collision() {
        let (conn, _dir) = attach_lake("prefix-bad");
        conn.execute_batch("CREATE TABLE __datamk_junk (id INTEGER);")
            .unwrap();
        let err = check_reserved_prefix(&conn).unwrap_err().to_string();
        assert!(err.contains("__datamk_junk"), "got: {err}");
        assert!(err.contains("reserved"), "got: {err}");
        assert!(err.contains("__datamk_watermarks"), "got: {err}");
    }

    #[test]
    fn reserved_prefix_check_ignores_the_watermark_table_itself() {
        let (conn, _dir) = attach_lake("prefix-watermarks");
        conn.execute_batch(
            "CREATE TABLE __datamk_watermarks ( \
               source VARCHAR NOT NULL, cursor_column VARCHAR NOT NULL, \
               mark_ts TIMESTAMPTZ, mark_date DATE, mark_int BIGINT, last_delta_rows BIGINT);",
        )
        .unwrap();
        check_reserved_prefix(&conn).expect("the watermark table itself must never be flagged");
    }

    // ADR 0005 §2 item 5: the grain-violation error names the likely
    // replay-safety cause — but only in a cell that declares an incremental
    // source; a plain cell keeps the plain message.
    fn grain_violation_cell(incremental: bool) -> CellDef {
        let inc = if incremental {
            "\n    incremental:\n      cursor: updated_at"
        } else {
            ""
        };
        serde_yaml::from_str(&format!(
            r#"
cell: c
sources:
  events:
    connection: crm
    table: analytics.events{inc}
interface:
  - name: dup
    version: 1.0.0
    grain: [id]
"#
        ))
        .unwrap()
    }

    fn lake_with_duplicate_grain(tag: &str) -> (Connection, PathBuf) {
        let (conn, dir) = attach_lake(tag);
        conn.execute_batch("CREATE TABLE dup AS SELECT 1 AS id UNION ALL SELECT 1;")
            .unwrap();
        (conn, dir)
    }

    #[test]
    fn grain_violation_names_the_incremental_cause_when_one_is_declared() {
        let (conn, _dir) = lake_with_duplicate_grain("grain-hint");
        let err = check(&conn, &grain_violation_cell(true), &HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("grain"), "got: {err}");
        assert!(err.contains("not replay-safe"), "got: {err}");
        assert!(err.contains("docs/guides/incremental.md"), "got: {err}");
    }

    #[test]
    fn grain_violation_stays_plain_without_incremental_sources() {
        let (conn, _dir) = lake_with_duplicate_grain("grain-plain");
        let err = check(&conn, &grain_violation_cell(false), &HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not unique"), "got: {err}");
        assert!(!err.contains("replay-safe"), "got: {err}");
    }

    // ADR 0012 §3 ratchet check 4: supported => non-empty description; the
    // friction lands exactly on the deliberate promotion gesture —
    // experimental exports need nothing.
    #[test]
    fn supported_export_without_a_description_fails_the_lint() {
        let def: CellDef = serde_yaml::from_str(
            "cell: c\ninterface:\n  - name: e\n    version: 1.0.0\n    contract: supported\n",
        )
        .unwrap();
        let err = check_supported_have_descriptions(&def, &HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("requires a non-empty `description`"),
            "got: {err}"
        );

        let def: CellDef = serde_yaml::from_str(
            "cell: c\ninterface:\n  - name: e\n    version: 1.0.0\n    contract: supported\n    description: \"   \"\n",
        )
        .unwrap();
        assert!(
            check_supported_have_descriptions(&def, &HashMap::new()).is_err(),
            "whitespace-only description must not satisfy the lint"
        );
    }

    /// ADR 0013: `docs:` is additive, never a substitute — a supported
    /// export with a `docs:` page but no `description` still fails the
    /// lint, and the message says so explicitly.
    #[test]
    fn docs_page_does_not_satisfy_the_supported_description_lint() {
        let def: CellDef = serde_yaml::from_str(
            "cell: c\ninterface:\n  - name: e\n    version: 1.0.0\n    contract: supported\n    docs: docs/e.md\n",
        )
        .unwrap();
        let err = check_supported_have_descriptions(&def, &HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("a `docs:` page does not satisfy it"),
            "got: {err}"
        );

        // Setting both is correct, not an error.
        let def: CellDef = serde_yaml::from_str(
            "cell: c\ninterface:\n  - name: e\n    version: 1.0.0\n    contract: supported\n    description: One row per thing.\n    docs: docs/e.md\n",
        )
        .unwrap();
        check_supported_have_descriptions(&def, &HashMap::new())
            .expect("description + docs together must pass");
    }

    #[test]
    fn experimental_exports_need_no_description_and_described_supported_pass() {
        let def: CellDef = serde_yaml::from_str(
            "cell: c\ninterface:\n  - name: a\n    version: 1.0.0\n  - name: b\n    version: 1.0.0\n    contract: supported\n    description: One row per thing.\n",
        )
        .unwrap();
        check_supported_have_descriptions(&def, &HashMap::new()).expect("lint must pass");
    }

    /// Issue #18: a `contract: supported` bound export with no local
    /// `description:` still passes when the bound source has at least one
    /// warehouse-documented column in the recorded source descriptions — the
    /// meaning is available, just not restated in `cell.yaml`. Forcing a
    /// local copy here is exactly the rot `datamk interface import` exists
    /// to avoid creating.
    #[test]
    fn supported_bound_export_passes_when_the_source_has_warehouse_descriptions() {
        let def: CellDef = serde_yaml::from_str(
            "cell: c\n\
             interface:\n\
             \x20 - name: e\n\
             \x20   version: 1.0.0\n\
             \x20   bind: raw\n\
             \x20   contract: supported\n",
        )
        .unwrap();
        let mut warehouse = HashMap::new();
        warehouse.insert(
            "raw".to_string(),
            crate::engine::SourceWarehouseColumns {
                connector: "bigquery",
                columns: indexmap::IndexMap::new(),
                descriptions: indexmap::IndexMap::from([(
                    "customer_id".to_string(),
                    "The customer's unique identifier.".to_string(),
                )]),
            },
        );
        check_supported_have_descriptions(&def, &warehouse)
            .expect("meaning documented at the source must satisfy the lint");
    }

    /// The other half: a `contract: supported` bound export whose source has
    /// no warehouse descriptions at all (or isn't in the map — e.g. a
    /// connector with no metadata job, ADR 0010) still fails the lint
    /// exactly as before — "bound" alone is not an exemption, only
    /// documented meaning is.
    #[test]
    fn supported_bound_export_with_no_warehouse_descriptions_still_fails_the_lint() {
        let def: CellDef = serde_yaml::from_str(
            "cell: c\n\
             interface:\n\
             \x20 - name: e\n\
             \x20   version: 1.0.0\n\
             \x20   bind: raw\n\
             \x20   contract: supported\n",
        )
        .unwrap();
        // No entry for "raw" at all (e.g. a Postgres/Snowflake connector, or
        // a raw file — no metadata job, ADR 0010).
        let err = check_supported_have_descriptions(&def, &HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("requires a non-empty `description`"),
            "got: {err}"
        );

        // An entry that exists but carries no descriptions (a classified
        // BigQuery source with genuinely no documented columns) must not
        // satisfy the lint either.
        let mut warehouse = HashMap::new();
        warehouse.insert(
            "raw".to_string(),
            crate::engine::SourceWarehouseColumns {
                connector: "bigquery",
                columns: indexmap::IndexMap::new(),
                descriptions: indexmap::IndexMap::new(),
            },
        );
        assert!(
            check_supported_have_descriptions(&def, &warehouse).is_err(),
            "an empty-descriptions warehouse entry must not satisfy the lint"
        );
    }

    // ADR 0012 §3 ratchet check 2 (orphan-kill, by construction): a
    // description rides the schema entry, so describing a column the source
    // no longer has IS the existing declared-column-missing hard error —
    // rename/drop kills the orphaned sentence.
    #[test]
    fn described_column_missing_from_the_source_is_still_a_hard_error() {
        let (conn, _dir) = attach_lake("orphan-desc");
        conn.execute_batch("CREATE TABLE t AS SELECT 1 AS id;")
            .unwrap();
        let def: CellDef = serde_yaml::from_str(
            "cell: c\n\
             interface:\n\
             \x20 - name: t\n\
             \x20   version: 1.0.0\n\
             \x20   schema:\n\
             \x20     id: integer\n\
             \x20     dropped_col:\n\
             \x20       type: decimal\n\
             \x20       description: A sentence about a column that no longer exists.\n",
        )
        .unwrap();
        let err = check(&conn, &def, &HashMap::new()).unwrap_err().to_string();
        assert!(
            err.contains("declared column 'dropped_col' missing"),
            "got: {err}"
        );
    }

    // ADR 0012 §7: a declared-type mismatch is a hard verify error, not a
    // warning — the breaking-change promotion, pinned end to end against a
    // real table.
    #[test]
    fn declared_type_mismatch_is_a_hard_error() {
        let (conn, _dir) = attach_lake("type-mismatch");
        conn.execute_batch("CREATE TABLE t AS SELECT 1 AS id, 'x' AS label;")
            .unwrap();
        let def: CellDef = serde_yaml::from_str(
            "cell: c\n\
             interface:\n\
             \x20 - name: t\n\
             \x20   version: 1.0.0\n\
             \x20   schema:\n\
             \x20     id: integer\n\
             \x20     label: decimal\n",
        )
        .unwrap();
        let err = check(&conn, &def, &HashMap::new()).unwrap_err().to_string();
        assert!(err.contains("declared type 'decimal'"), "got: {err}");
        assert!(err.contains("column 'label'"), "got: {err}");
        assert!(
            err.contains("promoted to an error by ADR 0012"),
            "got: {err}"
        );
    }

    // Issue #6/#9: `column_type_ok` wired end to end through `check`, not
    // just the pure `bigquery::type_compatible` vocabulary in isolation.
    // Reproduces the commit's motivating case — a wide BigQuery
    // NUMERIC/BIGNUMERIC value that DuckDB, once attached, can only render
    // as VARCHAR (the DuckDB-side half of that degradation is reproducible
    // locally without credentials; the warehouse round trip that would have
    // produced it in the first place is not, and this test does not claim
    // to exercise that round trip). Without a warehouse-native authority,
    // the declared `decimal` fails against the degraded VARCHAR exactly as
    // it always has; with one claiming the real BigQuery type is NUMERIC,
    // the same VARCHAR-rendered column passes — proving `check` compares
    // against the warehouse authority when one is present for a bound
    // export's source, not DuckDB's `DESCRIBE`.
    #[test]
    fn a_bound_exports_declared_decimal_passes_against_warehouse_numeric_despite_a_degraded_duckdb_varchar(
    ) {
        let (conn, _dir) = attach_lake("warehouse-numeric");
        conn.execute_batch(
            "CREATE VIEW raw AS SELECT 1 AS id, \
             '123456789012345678901234567890.5' AS amount;",
        )
        .unwrap();
        let def: CellDef = serde_yaml::from_str(
            "cell: c\n\
             interface:\n\
             \x20 - name: t\n\
             \x20   version: 1.0.0\n\
             \x20   bind: raw\n\
             \x20   schema:\n\
             \x20     id: integer\n\
             \x20     amount: decimal\n",
        )
        .unwrap();

        // No warehouse authority: DuckDB's own DESCRIBE sees the degraded
        // VARCHAR, and the declared `decimal` fails, same as before this
        // commit.
        let err = check(&conn, &def, &HashMap::new()).unwrap_err().to_string();
        assert!(err.contains("declared type 'decimal'"), "got: {err}");
        assert!(err.contains("actual type 'VARCHAR'"), "got: {err}");

        // With a warehouse authority for this export's bound source: the
        // same VARCHAR-rendered column passes, because BigQuery's own
        // native type — NUMERIC — is what actually gets compared.
        let mut warehouse = HashMap::new();
        warehouse.insert(
            "raw".to_string(),
            crate::engine::SourceWarehouseColumns {
                connector: "bigquery",
                columns: indexmap::IndexMap::from([("amount".to_string(), "NUMERIC".to_string())]),
                descriptions: indexmap::IndexMap::new(),
            },
        );
        check(&conn, &def, &warehouse)
            .expect("warehouse-native NUMERIC must pass a declared decimal");
    }

    // The fallback half of the same wiring: a bound export whose source has
    // no entry in `warehouse_columns` at all (a raw-file bind, or a
    // connector — Postgres/Snowflake — that never populates `ObjectMeta.
    // columns`) must keep using DuckDB's own `DESCRIBE` as the authority,
    // not silently pass because a warehouse map merely exists.
    #[test]
    fn a_bound_export_with_no_warehouse_entry_still_uses_duckdbs_own_type() {
        let (conn, _dir) = attach_lake("no-warehouse-entry");
        conn.execute_batch("CREATE VIEW raw AS SELECT 1 AS id, 'x' AS amount;")
            .unwrap();
        let def: CellDef = serde_yaml::from_str(
            "cell: c\n\
             interface:\n\
             \x20 - name: t\n\
             \x20   version: 1.0.0\n\
             \x20   bind: raw\n\
             \x20   schema:\n\
             \x20     id: integer\n\
             \x20     amount: decimal\n",
        )
        .unwrap();

        // A non-empty warehouse map that simply has no entry for THIS
        // export's source — e.g. another export's bound BigQuery table.
        let mut warehouse = HashMap::new();
        warehouse.insert(
            "some_other_source".to_string(),
            crate::engine::SourceWarehouseColumns {
                connector: "bigquery",
                columns: indexmap::IndexMap::from([("amount".to_string(), "NUMERIC".to_string())]),
                descriptions: indexmap::IndexMap::new(),
            },
        );
        let err = check(&conn, &def, &warehouse).unwrap_err().to_string();
        assert!(err.contains("declared type 'decimal'"), "got: {err}");
        assert!(err.contains("actual type 'VARCHAR'"), "got: {err}");
    }

    #[test]
    fn matching_declared_types_still_verify_cleanly() {
        let (conn, _dir) = attach_lake("type-match");
        conn.execute_batch("CREATE TABLE t AS SELECT 1::INTEGER AS id, 'x' AS label;")
            .unwrap();
        let def: CellDef = serde_yaml::from_str(
            "cell: c\n\
             interface:\n\
             \x20 - name: t\n\
             \x20   version: 1.0.0\n\
             \x20   schema:\n\
             \x20     id: integer\n\
             \x20     label: string\n",
        )
        .unwrap();
        check(&conn, &def, &HashMap::new()).expect("compatible declared types must pass");
    }

    #[test]
    fn string_aliases_match_varchar() {
        assert!(type_compatible("string", "VARCHAR"));
        assert!(type_compatible("varchar", "VARCHAR(255)"));
        assert!(type_compatible("text", "TEXT"));
        assert!(!type_compatible("string", "INTEGER"));
    }

    #[test]
    fn integer_widths_are_distinguished() {
        assert!(type_compatible("int", "INTEGER"));
        assert!(type_compatible("integer", "INT32"));
        assert!(type_compatible("bigint", "BIGINT"));
        assert!(type_compatible("long", "INT64"));
        // A declared int should not silently match a bigint column.
        assert!(!type_compatible("int", "BIGINT"));
    }

    #[test]
    fn numeric_and_float_families() {
        assert!(type_compatible("decimal", "DECIMAL(18,2)"));
        assert!(type_compatible("numeric", "NUMERIC(10,0)"));
        assert!(type_compatible("double", "DOUBLE"));
        assert!(type_compatible("float", "REAL"));
    }

    #[test]
    fn temporal_and_boolean() {
        assert!(type_compatible("date", "DATE"));
        assert!(type_compatible("timestamp", "TIMESTAMP WITH TIME ZONE"));
        assert!(type_compatible("bool", "BOOLEAN"));
        assert!(type_compatible("boolean", "BOOLEAN"));
        assert!(!type_compatible("date", "TIMESTAMP"));
    }

    #[test]
    fn declared_type_matching_is_case_insensitive() {
        assert!(type_compatible("STRING", "varchar"));
        assert!(type_compatible("Integer", "INTEGER"));
    }

    #[test]
    fn unknown_declared_type_falls_back_to_case_insensitive_equality() {
        assert!(type_compatible("uuid", "UUID"));
        assert!(type_compatible("uuid", "uuid"));
        assert!(!type_compatible("uuid", "VARCHAR"));
    }

    /// Issue #18: `duckdb_declared_type_for` (the inverse `datamk interface
    /// import` uses for a raw file or a no-metadata-job connector) must
    /// never emit a declared name its own forward sibling, `type_compatible`,
    /// would then reject. Offline, no DuckDB connection needed.
    #[test]
    fn duckdb_declared_type_for_round_trips_through_type_compatible() {
        let actuals = [
            "VARCHAR",
            "VARCHAR(255)",
            "TEXT",
            "INTEGER",
            "INT",
            "INT32",
            "BIGINT",
            "INT64",
            "DECIMAL(18,2)",
            "NUMERIC(10,0)",
            "DOUBLE",
            "FLOAT",
            "REAL",
            "BOOLEAN",
            "DATE",
            "TIMESTAMP",
            "TIMESTAMP WITH TIME ZONE",
        ];
        for actual in actuals {
            let declared = duckdb_declared_type_for(actual)
                .unwrap_or_else(|| panic!("expected a declared name for actual type {actual}"));
            assert!(
                type_compatible(declared, actual),
                "duckdb_declared_type_for({actual}) = {declared}, but type_compatible({declared}, \
                 {actual}) is false — the two functions have drifted apart"
            );
        }
    }

    /// A DuckDB type with no clean declared name comes back `None`, never a
    /// guess — the caller's contract is `type: unmapped`.
    #[test]
    fn duckdb_declared_type_for_returns_none_for_an_unmappable_type() {
        for actual in [
            "STRUCT(a INTEGER)",
            "BLOB",
            "TIME",
            "UUID",
            "MAP(VARCHAR, INTEGER)",
        ] {
            assert_eq!(
                duckdb_declared_type_for(actual),
                None,
                "expected no declared name for {actual}"
            );
        }
    }

    // --- ADR 0008 Consequences: declarative grain inheritance --------------

    fn materialize_transform(table: &str, key: &[&str]) -> ResolvedTransform {
        ResolvedTransform {
            sql: format!("sql/{table}.sql"),
            strategy: crate::config::MaterializeStrategy::Upsert,
            key: key.iter().map(|s| s.to_string()).collect(),
            table: table.to_string(),
        }
    }

    fn cell_with_export(yaml_export: &str) -> CellDef {
        serde_yaml::from_str(&format!("cell: t\ninterface:\n{yaml_export}\n")).unwrap()
    }

    #[test]
    fn omitted_grain_inherits_the_materialize_key() {
        let mut def = cell_with_export(
            "  - name: fct_flights\n    version: 1.0.0\n    source: fct_flights\n",
        );
        let transforms = vec![materialize_transform("fct_flights", &["flight_id"])];
        apply_declarative_grain_inheritance(&mut def, &transforms).unwrap();
        assert_eq!(def.interface[0].grain, vec!["flight_id".to_string()]);
    }

    #[test]
    fn omitted_grain_inherits_a_composite_materialize_key() {
        let mut def = cell_with_export("  - name: fct\n    version: 1.0.0\n    source: fct\n");
        let transforms = vec![materialize_transform("fct", &["a", "b"])];
        apply_declarative_grain_inheritance(&mut def, &transforms).unwrap();
        assert_eq!(
            def.interface[0].grain,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn export_source_defaults_to_name_for_inheritance_matching_too() {
        // `source:` omitted -> defaults to the export name (`source_object`)
        // — inheritance must resolve through that default, not require an
        // explicit `source:` restating the name.
        let mut def = cell_with_export("  - name: fct\n    version: 1.0.0\n");
        let transforms = vec![materialize_transform("fct", &["id"])];
        apply_declarative_grain_inheritance(&mut def, &transforms).unwrap();
        assert_eq!(def.interface[0].grain, vec!["id".to_string()]);
    }

    #[test]
    fn explicit_grain_extending_the_key_is_kept_as_declared() {
        let mut def = cell_with_export(
            "  - name: fct\n    version: 1.0.0\n    source: fct\n    grain: [id, region]\n",
        );
        let transforms = vec![materialize_transform("fct", &["id"])];
        apply_declarative_grain_inheritance(&mut def, &transforms).unwrap();
        assert_eq!(
            def.interface[0].grain,
            vec!["id".to_string(), "region".to_string()]
        );
    }

    #[test]
    fn explicit_grain_missing_a_key_column_errors() {
        let mut def = cell_with_export(
            "  - name: fct\n    version: 1.0.0\n    source: fct\n    grain: [region]\n",
        );
        let transforms = vec![materialize_transform("fct", &["id"])];
        let err = apply_declarative_grain_inheritance(&mut def, &transforms)
            .unwrap_err()
            .to_string();
        assert!(err.contains("export 'fct'"), "got: {err}");
        assert!(
            err.contains("does not contain materialize key"),
            "got: {err}"
        );
        assert!(err.contains("\"id\""), "got: {err}");
        assert!(err.contains("never be coarser"), "got: {err}");
    }

    #[test]
    fn explicit_grain_missing_one_of_two_key_columns_errors() {
        let mut def = cell_with_export(
            "  - name: fct\n    version: 1.0.0\n    source: fct\n    grain: [a]\n",
        );
        let transforms = vec![materialize_transform("fct", &["a", "b"])];
        let err = apply_declarative_grain_inheritance(&mut def, &transforms)
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"b\""), "got: {err}");
    }

    #[test]
    fn export_with_no_matching_transform_table_is_left_untouched() {
        // No transform in the cell claims this export's source table at all
        // — grain (including an empty one) is exactly as declared.
        let mut def = cell_with_export("  - name: orphan_export\n    version: 1.0.0\n");
        apply_declarative_grain_inheritance(&mut def, &[]).unwrap();
        assert!(def.interface[0].grain.is_empty());
    }

    // ADR 0008 work item 4: "confirm the §2 raw-path guards are unchanged
    // and never fire on a correct declarative transform" — end to end, not
    // just at the pure-inheritance level: parse a declarative `cell.yaml`,
    // resolve its transforms, apply inheritance (mirroring `config::load`'s
    // exact sequence), then run the real, DB-connected `check()` against an
    // actual key-unique table. The grain-uniqueness backstop must pass
    // using the *inherited* grain — nothing about it is declarative-aware,
    // and it doesn't need to be.
    #[test]
    fn check_passes_for_a_declarative_export_using_inherited_grain_uniqueness() {
        let (conn, _dir) = attach_lake("declarative-grain-check");
        conn.execute_batch(
            "CREATE TABLE fct_flights (flight_id INTEGER, carrier VARCHAR); \
             INSERT INTO fct_flights VALUES (1, 'AA'), (2, 'BA');",
        )
        .unwrap();
        let mut def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             transforms:\n\
             \x20 - sql: sql/fct_flights.sql\n\
             \x20   materialize: upsert\n\
             \x20   key: [flight_id]\n\
             interface:\n\
             \x20 - name: fct_flights\n\
             \x20   version: 1.0.0\n",
        )
        .unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        apply_declarative_grain_inheritance(&mut def, &transforms).unwrap();
        assert_eq!(def.interface[0].grain, vec!["flight_id".to_string()]);

        check(&conn, &def, &HashMap::new())
            .expect("declarative export with inherited grain must verify cleanly");
    }

    // --- issue #6, binding model: bound exports and standalone `check` ------

    #[test]
    fn check_names_the_binding_when_a_bound_export_has_no_live_view() {
        // Issue #6 (binding model): `check` no longer silently skips a bound
        // export with no live view — every caller (`run`, standalone
        // `verify`) is now responsible for binding it first (`engine::
        // bind_sources`), so a missing view here is a genuine failure of
        // that binding pass, not an expected absence. The error must still
        // be legible, though — it names the binding and where it should
        // have happened rather than surfacing DuckDB's raw "table does not
        // exist".
        let (conn, _dir) = attach_lake("bound-no-view");
        let def: CellDef = serde_yaml::from_str(
            "cell: t\ninterface:\n  - name: virtual_pii\n    version: 1.0.0\n    bind: raw\n",
        )
        .unwrap();
        let err = check(&conn, &def, &HashMap::new()).unwrap_err().to_string();
        assert!(
            err.contains("bind: raw") && err.contains("no live view"),
            "a missing bound view must name the binding and the gap, not just DuckDB's raw \
             error: got {err}"
        );
    }

    #[test]
    fn check_still_validates_a_bound_export_when_its_view_is_bound() {
        // The in-run shape (issue #6): once `engine::bind_sources` has left
        // the declared source's TEMP VIEW in this session, `check` validates
        // it exactly like any other export — schema, grain existence, grain
        // uniqueness — the bound-export branch only changes behavior on a
        // MISSING relation.
        let (conn, _dir) = attach_lake("bound-and-checked");
        conn.execute_batch("CREATE TEMP VIEW raw AS SELECT * FROM (VALUES (1), (1)) AS t(id);")
            .unwrap();
        let def: CellDef = serde_yaml::from_str(
            "cell: t\ninterface:\n  - name: virtual_pii\n    version: 1.0.0\n    grain: [id]\n    bind: raw\n",
        )
        .unwrap();
        let err = check(&conn, &def, &HashMap::new()).unwrap_err().to_string();
        assert!(
            err.contains("is not unique"),
            "a bound export's live grain violation must still be caught: got {err}"
        );
    }

    // --- issue #6, live-verify core: standalone `verify::run` end-to-end ----

    /// A fresh, on-disk cell dir with `tag` keeping parallel tests apart —
    /// the `release.rs` test pattern (a real `cell.yaml`/`profiles/local.yaml`
    /// on disk, driven through the public command functions), used here
    /// because the live-verify bind pass needs a real `config::load` +
    /// `engine::open`, not the in-memory `attach_lake` fixture the `check`
    /// unit tests above use.
    fn live_verify_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datamk-live-verify-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sql")).unwrap();
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::write(
            dir.join("profiles/local.yaml"),
            "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
        )
        .unwrap();
        dir
    }

    fn write_csv(dir: &Path, name: &str, rows: &[(i64, &str)]) {
        let mut body = "id,val\n".to_string();
        for (id, val) in rows {
            body.push_str(&format!("{id},{val}\n"));
        }
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn standalone_verify_on_an_all_bound_cell_binds_live_and_catches_a_broken_grain() {
        // Issue #6, binding model: an all-bound cell has no snapshot and can
        // never build one (`run` refuses — `builds_no_snapshot`) — this is
        // the case the whole live-verify slice exists for. Standalone
        // `verify::run` must bind `data.csv` as a live source and run the
        // real schema+grain checks against it directly — not skip.
        let dir = live_verify_dir("all-bound-clean");
        std::fs::write(
            dir.join("cell.yaml"),
            "cell: virtual_only\n\
             interface:\n\
             \x20 - name: virtual_pii\n\
             \x20   version: 1.0.0\n\
             \x20   grain: [id]\n\
             \x20   bind: raw\n\
             sources:\n\
             \x20 raw: ./data.csv\n",
        )
        .unwrap();
        write_csv(&dir, "data.csv", &[(1, "a"), (2, "b")]);

        let file = dir.join("cell.yaml");
        run(&file, "local").expect("live-verify of a clean all-bound cell must pass");
        assert!(
            dir.join(".cell/source_check.json").is_file(),
            "a passing live check must leave a .cell/source_check.json record behind"
        );

        // Now break the grain at the source and confirm the live check
        // actually catches it — not a stale skip, a real, running check.
        write_csv(&dir, "data.csv", &[(1, "a"), (1, "b")]);
        let err = run(&file, "local").unwrap_err().to_string();
        assert!(
            err.contains("is not unique"),
            "a broken grain at the live source must fail verify with the usual grain-violation \
             voice: got {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn standalone_verify_on_a_mixed_cell_checks_the_bound_export_live_and_the_materialized_export_against_the_lake(
    ) {
        // Issue #6, binding model: a mixed cell (one materializing
        // transform, one bound export) must have BOTH halves checked by a
        // standalone verify — the materialized export against the lake,
        // exactly as before, and the bound export live, freshly bound.
        let dir = live_verify_dir("mixed");
        std::fs::write(
            dir.join("cell.yaml"),
            "cell: mixed\n\
             transforms:\n\
             \x20 - sql: sql/stg.sql\n\
             \x20   materialize: replace\n\
             interface:\n\
             \x20 - name: stg\n\
             \x20   version: 1.0.0\n\
             \x20   grain: [id]\n\
             \x20 - name: virtual_pii\n\
             \x20   version: 1.0.0\n\
             \x20   grain: [id]\n\
             \x20   bind: raw\n\
             sources:\n\
             \x20 raw: ./data.csv\n",
        )
        .unwrap();
        std::fs::write(dir.join("sql/stg.sql"), "SELECT * FROM raw").unwrap();
        write_csv(&dir, "data.csv", &[(1, "a"), (2, "b")]);

        let file = dir.join("cell.yaml");
        // The materializing half needs a real build first — unlike the
        // all-bound case, `run` does not refuse a mixed cell, and the
        // materialized export genuinely has nothing to check without one.
        crate::engine::run(&file, "local", None, crate::engine::RunOptions::default())
            .expect("build the mixed cell");
        run(&file, "local").expect("live-verify of a clean mixed cell must pass");

        // Break the bound export live (the source, re-read fresh on every
        // verify) without touching the lake at all.
        write_csv(&dir, "data.csv", &[(1, "a"), (1, "b")]);
        let err = run(&file, "local").unwrap_err().to_string();
        assert!(
            err.contains("export 'virtual_pii'") && err.contains("is not unique"),
            "the bound export's live grain violation must be caught: got {err}"
        );

        // Restore the source, then corrupt the MATERIALIZED table directly
        // in the lake (not through a transform — pinning that a standalone
        // verify still checks the lake for the materializing half, exactly
        // as it did before this feature).
        write_csv(&dir, "data.csv", &[(1, "a"), (2, "b")]);
        let cell = crate::engine::open(&file, "local", false)
            .expect("re-open the lake, writable, to corrupt it directly");
        cell.conn
            .execute_batch("INSERT INTO stg VALUES (1, 'dup');")
            .expect("insert a duplicate key directly into the materialized table");
        drop(cell);
        let err = run(&file, "local").unwrap_err().to_string();
        assert!(
            err.contains("export 'stg'") && err.contains("is not unique"),
            "the materialized export's grain violation must still be caught against the lake: \
             got {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- ADR 0008 decision 5: the no-grain warning as the removed-entry -----
    // --- backstop -------------------------------------------------------

    #[test]
    fn a_table_with_no_materialize_entry_and_no_declared_grain_falls_back_to_the_no_grain_warning()
    {
        // `sources_without_grain_backstop` looks only at `def.interface`/
        // `def.sources` — it has no view of `transforms:` at all. This
        // pins the case that matters in practice: a table whose
        // `materialize:` entry was removed from `transforms:` (the table
        // now lives on in the lake, built by an earlier run, or managed
        // outside the pipeline entirely — ADR 0008 decision 8) but whose
        // export still serves it and never restated its own `grain:` — it
        // was relying on inheritance, which requires a live `materialize:`
        // entry (`apply_declarative_grain_inheritance`). It now has none,
        // which is exactly the shape `sources_without_grain_backstop` (ADR
        // 0005 §2 item 3) already exists to catch, as long as the cell's
        // source is a detectable `incremental:` connection.
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             sources:\n\
             \x20 events:\n\
             \x20   connection: crm\n\
             \x20   table: analytics.events\n\
             \x20   incremental:\n\
             \x20     cursor: updated_at\n\
             interface:\n\
             \x20 - name: fct_events\n\
             \x20   version: 1.0.0\n",
        )
        .unwrap();
        // No `transforms:` entry builds `fct_events` (removed, or never
        // this cell's to build) and the export declares no grain — never
        // restated the key it used to inherit from a live `materialize:`
        // entry.
        assert_eq!(
            sources_without_grain_backstop(&def),
            vec!["events"],
            "an export with no live `materialize:` entry to inherit grain from, and no \
             restated `grain:` of its own, must still trip the no-grain backstop when the \
             cell's source is a detectable `incremental:` connection"
        );
    }

    #[test]
    fn declaring_grain_explicitly_clears_the_no_grain_warning_with_no_materialize_entry() {
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             sources:\n\
             \x20 events:\n\
             \x20   connection: crm\n\
             \x20   table: analytics.events\n\
             \x20   incremental:\n\
             \x20     cursor: updated_at\n\
             interface:\n\
             \x20 - name: fct_events\n\
             \x20   version: 1.0.0\n\
             \x20   grain: [event_id]\n",
        )
        .unwrap();
        assert!(sources_without_grain_backstop(&def).is_empty());
    }

    #[test]
    fn a_purely_local_cell_with_no_incremental_source_has_no_grain_backstop_at_all() {
        // Documented, pre-existing limit (ADR 0005 §2 item 3), not
        // something ADR 0008 introduces or is required to close: the
        // no-grain backstop is scoped to `incremental:` connections
        // specifically — the one accumulation shape the engine can detect
        // without parsing SQL. A cell with no incremental connection at
        // all — e.g. this repo's own `init` scaffold, which reads only
        // synthesized local VALUES — gets no warning here, regardless of
        // whether an export's source used to be declarative. Pinned as a
        // known gap, not silently assumed away.
        let def: CellDef = serde_yaml::from_str(
            "cell: t\ntransforms:\n  - sql/fct.sql\ninterface:\n  - name: fct\n    version: 1.0.0\n",
        )
        .unwrap();
        assert!(sources_without_grain_backstop(&def).is_empty());
    }

    // --- ADR 0008 guard 4c: the per-model incremental-source name scan -----

    fn replace_transform(table: &str) -> ResolvedTransform {
        ResolvedTransform {
            sql: format!("sql/{table}.sql"),
            strategy: MaterializeStrategy::Replace,
            key: vec![],
            table: table.to_string(),
        }
    }

    /// A temp cell directory with `cell.yaml` and the given `sql/` files —
    /// guard 4c reads each `replace` model's actual file text (metadata
    /// scanning, never SQL parsing), so its tests need real files on disk,
    /// unlike the old cell-wide ban this replaced.
    fn gate_test_dir(tag: &str, cell_yaml: &str, sql_files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datamk-verify-gate-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sql")).unwrap();
        std::fs::write(dir.join("cell.yaml"), cell_yaml).unwrap();
        for (f, sql) in sql_files {
            std::fs::write(dir.join("sql").join(f), sql).unwrap();
        }
        dir
    }

    fn incremental_events_source_yaml() -> &'static str {
        "sources:\n\
         \x20 events:\n\
         \x20   connection: crm\n\
         \x20   table: analytics.events\n\
         \x20   incremental:\n\
         \x20     cursor: updated_at\n"
    }

    #[test]
    fn check_replace_incremental_gate_fires_on_a_replace_model_referencing_the_delta_source() {
        let dir = gate_test_dir(
            "fires",
            &format!(
                "cell: t\n{}transforms:\n  - sql: sql/rollup.sql\n    materialize: replace\n",
                incremental_events_source_yaml()
            ),
            &[("rollup.sql", "SELECT count(*) AS n FROM events")],
        );
        let def: CellDef =
            serde_yaml::from_str(&std::fs::read_to_string(dir.join("cell.yaml")).unwrap()).unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        let err = check_replace_incremental_gate(&def, &dir, &transforms)
            .unwrap_err()
            .to_string();
        // Exact text match — this is the gate's whole job, and the
        // coordinator asked for the implemented text to be reportable
        // verbatim, so pin it exactly rather than substring-checking.
        assert_eq!(
            err,
            "transform 'sql/rollup.sql': materialize: replace references incremental source \
             'events' — rebuilding from the delta would replace the table's history with just \
             the delta (truncation). Read the accumulated table instead (an upsert/append \
             model over 'events' in this cell), or change this model to materialize: \
             upsert/append if it should itself accumulate. See docs/guides/incremental.md §4."
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_replace_incremental_gate_is_silent_when_replace_reads_an_accumulator_in_the_same_cell()
    {
        // This is the case the old cell-wide ban wrongly forbade — pinned
        // per the coordinator's explicit instruction. `fct_events` contains
        // "events" as a substring but not as a word-boundary token (it's
        // preceded by `_`, an identifier character), so the accumulator
        // table itself never trips the scan.
        let dir = gate_test_dir(
            "silent-accumulator",
            &format!(
                "cell: t\n{}transforms:\n\
                 \x20 - sql: sql/fct_events.sql\n\
                 \x20   materialize: upsert\n\
                 \x20   key: [event_id]\n\
                 \x20 - sql: sql/daily_rollup.sql\n\
                 \x20   materialize: replace\n",
                incremental_events_source_yaml()
            ),
            &[
                ("fct_events.sql", "SELECT * FROM events"),
                ("daily_rollup.sql", "SELECT count(*) AS n FROM fct_events"),
            ],
        );
        let def: CellDef =
            serde_yaml::from_str(&std::fs::read_to_string(dir.join("cell.yaml")).unwrap()).unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        check_replace_incremental_gate(&def, &dir, &transforms).expect(
            "a replace rollup reading an upsert accumulator's table, not the delta source \
             itself, must be legal even though the cell has an incremental source",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_replace_incremental_gate_comment_mention_is_a_documented_false_positive() {
        // The scan is metadata matching, not SQL parsing (ADR 0008 guard
        // 4c) — it cannot distinguish a comment from code. A source name
        // mentioned in a `--` comment trips the gate exactly like a real
        // reference would. This is accepted and documented, not silently
        // worked around: the fix is to delete or reword the comment (no
        // behavior changes — only text the scanner reads).
        let dir = gate_test_dir(
            "comment-false-positive",
            &format!(
                "cell: t\n{}transforms:\n  - sql: sql/rollup.sql\n    materialize: replace\n",
                incremental_events_source_yaml()
            ),
            &[(
                "rollup.sql",
                "-- TODO: eventually reconcile against events once backfilled\n\
                 SELECT count(*) AS n FROM daily_snapshot",
            )],
        );
        let def: CellDef =
            serde_yaml::from_str(&std::fs::read_to_string(dir.join("cell.yaml")).unwrap()).unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        let err = check_replace_incremental_gate(&def, &dir, &transforms)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("references incremental source 'events'"),
            "the comment-only mention still trips the scan (documented limitation): got {err}"
        );
        // Workaround: reword the comment so it no longer contains the bare
        // source name as a token — the SELECT itself never changes.
        std::fs::write(
            dir.join("sql/rollup.sql"),
            "-- TODO: eventually reconcile against upstream once backfilled\n\
             SELECT count(*) AS n FROM daily_snapshot",
        )
        .unwrap();
        check_replace_incremental_gate(&def, &dir, &transforms)
            .expect("rewording the comment (no code change) clears the false positive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_replace_incremental_gate_does_not_scan_upsert_or_append_models() {
        // upsert/append models are delta consumers by design — they
        // legitimately reference the source name in every real cell, so
        // the scan must never even look at their file text.
        let dir = gate_test_dir(
            "not-scanned-upsert",
            &format!(
                "cell: t\n{}transforms:\n\
                 \x20 - sql: sql/fct_events.sql\n\
                 \x20   materialize: upsert\n\
                 \x20   key: [event_id]\n",
                incremental_events_source_yaml()
            ),
            &[("fct_events.sql", "SELECT * FROM events")],
        );
        let def: CellDef =
            serde_yaml::from_str(&std::fs::read_to_string(dir.join("cell.yaml")).unwrap()).unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        check_replace_incremental_gate(&def, &dir, &transforms)
            .expect("upsert/append models reading the source directly are never scanned");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_replace_incremental_gate_does_not_scan_never_models() {
        // issue #6 exhaustive-match regression: `never` writes nothing to
        // the lake at all — there is no truncation risk for guard 4c to
        // catch — so it must be excluded from the scan exactly like
        // upsert/append, never accidentally join `replace` in it.
        let dir = gate_test_dir(
            "not-scanned-never",
            &format!(
                "cell: t\n{}transforms:\n\
                 \x20 - sql: sql/fct_events.sql\n\
                 \x20   materialize: never\n",
                incremental_events_source_yaml()
            ),
            &[("fct_events.sql", "SELECT * FROM events")],
        );
        let def: CellDef =
            serde_yaml::from_str(&std::fs::read_to_string(dir.join("cell.yaml")).unwrap()).unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        check_replace_incremental_gate(&def, &dir, &transforms)
            .expect("never models reading the source directly are never scanned");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_replace_incremental_gate_names_multiple_incremental_sources() {
        let dir = gate_test_dir(
            "multiple-sources",
            "cell: t\n\
             sources:\n\
             \x20 events:\n\
             \x20   connection: crm\n\
             \x20   table: analytics.events\n\
             \x20   incremental:\n\
             \x20     cursor: updated_at\n\
             \x20 signups:\n\
             \x20   connection: crm\n\
             \x20   table: analytics.signups\n\
             \x20   incremental:\n\
             \x20     cursor: id\n\
             transforms:\n\
             \x20 - sql: sql/rollup.sql\n\
             \x20   materialize: replace\n",
            &[("rollup.sql", "SELECT count(*) AS n FROM signups")],
        );
        let def: CellDef =
            serde_yaml::from_str(&std::fs::read_to_string(dir.join("cell.yaml")).unwrap()).unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        let err = check_replace_incremental_gate(&def, &dir, &transforms)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("references incremental source 'signups'"),
            "got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_replace_incremental_gate_is_silent_in_a_pure_derived_cell() {
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             transforms:\n\
             \x20 - sql: sql/rollup.sql\n\
             \x20   materialize: replace\n",
        )
        .unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        // No incremental source at all -> the gate returns before ever
        // reading a file, so a placeholder (nonexistent) dir is fine here.
        check_replace_incremental_gate(&def, Path::new("/nonexistent"), &transforms)
            .expect("no incremental source anywhere in the cell -> replace is legal");
    }

    #[test]
    fn check_replace_incremental_gate_is_silent_with_incremental_source_but_no_replace() {
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             sources:\n\
             \x20 events:\n\
             \x20   connection: crm\n\
             \x20   table: analytics.events\n\
             \x20   incremental:\n\
             \x20     cursor: updated_at\n\
             transforms:\n\
             \x20 - sql: sql/fct.sql\n\
             \x20   materialize: upsert\n\
             \x20   key: [id]\n",
        )
        .unwrap();
        let transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        // No `replace` entry at all -> the loop never reaches a file read
        // for any entry, so a placeholder (nonexistent) dir is fine here.
        check_replace_incremental_gate(&def, Path::new("/nonexistent"), &transforms)
            .expect("upsert alongside an incremental source is fine — only replace is scanned");
    }

    #[test]
    fn contains_word_token_matches_only_whole_tokens() {
        assert!(contains_word_token("SELECT * FROM events", "events"));
        assert!(contains_word_token("SELECT * FROM events e", "events"));
        assert!(!contains_word_token("SELECT * FROM fct_events", "events"));
        assert!(!contains_word_token("SELECT * FROM events_fct", "events"));
        assert!(!contains_word_token("SELECT * FROM other_table", "events"));
        assert!(contains_word_token("events", "events"));
        assert!(contains_word_token("(events)", "events"));
    }

    #[test]
    fn omitted_grain_over_a_replace_table_is_not_inherited() {
        let mut def =
            cell_with_export("  - name: rollup\n    version: 1.0.0\n    source: rollup\n");
        let transforms = vec![replace_transform("rollup")];
        apply_declarative_grain_inheritance(&mut def, &transforms).unwrap();
        assert!(
            def.interface[0].grain.is_empty(),
            "replace has no key to inherit — grain must stay exactly as declared (empty)"
        );
    }

    fn never_transform(table: &str) -> ResolvedTransform {
        ResolvedTransform {
            sql: format!("sql/{table}.sql"),
            strategy: MaterializeStrategy::Never,
            key: vec![],
            table: table.to_string(),
        }
    }

    #[test]
    fn omitted_grain_over_a_never_table_is_not_inherited() {
        // issue #6 exhaustive-match regression: `never` has no `key:` (same
        // as `replace`), so the grain-inheritance filter must exclude it —
        // not fall through to "contributes grain" by accident of the match
        // arm ordering.
        let mut def = cell_with_export(
            "  - name: virtual_pii\n    version: 1.0.0\n    source: virtual_pii\n",
        );
        let transforms = vec![never_transform("virtual_pii")];
        apply_declarative_grain_inheritance(&mut def, &transforms).unwrap();
        assert!(
            def.interface[0].grain.is_empty(),
            "never has no key to inherit — grain must stay exactly as declared (empty)"
        );
    }

    #[test]
    fn explicit_grain_over_a_replace_table_is_kept_as_declared_with_no_contains_key_check() {
        let mut def = cell_with_export(
            "  - name: rollup\n    version: 1.0.0\n    source: rollup\n    grain: [order_date, region]\n",
        );
        let transforms = vec![replace_transform("rollup")];
        apply_declarative_grain_inheritance(&mut def, &transforms).unwrap();
        assert_eq!(
            def.interface[0].grain,
            vec!["order_date".to_string(), "region".to_string()],
            "explicit grain over a replace-sourced export is untouched, exactly like raw"
        );
    }
}
