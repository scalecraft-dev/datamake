//! All Snowflake-specific connector code: extension load, secret + ATTACH,
//! table-path validation/folding/quoting, and error rewrites. Realized via
//! the DuckDB community `snowflake` extension, which reads over the Arrow
//! ADBC Snowflake driver — plain SQL, so tables and views read through one
//! uniform path (no BigQuery-style storage-vs-jobs split).
//!
//! Two Snowflake-shaped divergences from the BigQuery connector:
//!
//! - **Every read is staged.** The extension's attach-scan cannot survive
//!   arbitrary transform SQL (a bare `SELECT COUNT(*)` over the scan fails
//!   with pushdown enabled *and* disabled — live-verified), so no Snowflake
//!   source is ever bound as a read-through view. `classify_objects`
//!   synthesizes `ObjectKind::Query` for everything, with no metadata job:
//!   routing doesn't depend on the object's real kind, predicates render
//!   DuckDB-side (no native column types needed), and the staging read
//!   itself is the not-found detector (`rewrite_stage_error`).
//! - **The ADBC driver is a separate native library** the extension loads at
//!   attach (`libadbc_driver_snowflake`) — not installable from DuckDB's
//!   extension registry, so a missing driver gets an actionable rewrite
//!   (`rewrite_attach_error`) instead of the raw searched-paths error.

use anyhow::{bail, Result};
use indexmap::IndexMap;

use super::{quote, CursorPredicate, ObjectKind, ObjectMeta};
use crate::config::SnowflakeAuth;
use crate::engine::esc;

pub(super) const INSTALL_LOAD_SQL: &str = "INSTALL snowflake FROM community; LOAD snowflake;";

/// The session-local DuckDB secret a connection's ATTACH references, derived
/// from the attach alias so `attach_sql` and `query_read_sql` (which passes
/// it to `snowflake_query()`) can never disagree on the name.
fn secret_name(alias: &str) -> String {
    format!("{alias}_secret")
}

/// The secret + ATTACH batch. `CREATE OR REPLACE SECRET` + `ATTACH IF NOT
/// EXISTS` keyed on the connection name: a connection shared by several
/// sources attaches once, and re-running the batch is idempotent (the
/// secret is re-created per source — a local, session-scoped metadata write
/// with identical values; the already-attached connection is undisturbed).
/// `enable_pushdown true` is safe because the engine only ever sends
/// `SELECT *` (with an optional watermark predicate — pushdown of those
/// comparisons is live-verified byte-correct) through the scan; transforms
/// only ever see the staged copy.
pub(super) fn attach_sql(
    account: &str,
    database: &str,
    auth: &SnowflakeAuth,
    warehouse: Option<&str>,
    role: Option<&str>,
    alias: &str,
) -> String {
    let mut params = vec![
        "TYPE snowflake".to_string(),
        format!("ACCOUNT '{}'", esc(account)),
        format!("DATABASE '{}'", esc(database)),
    ];
    match auth {
        SnowflakeAuth::KeyPair {
            user,
            private_key_path,
            passphrase,
        } => {
            params.push(format!("USER '{}'", esc(user)));
            params.push("AUTH_TYPE 'key_pair'".to_string());
            params.push(format!("PRIVATE_KEY '{}'", esc(private_key_path)));
            if let Some(p) = passphrase {
                params.push(format!("PRIVATE_KEY_PASSPHRASE '{}'", esc(&p.0)));
            }
        }
        SnowflakeAuth::ExternalBrowser { user } => {
            params.push(format!("USER '{}'", esc(user)));
            params.push("AUTH_TYPE 'externalbrowser'".to_string());
        }
    }
    if let Some(w) = warehouse {
        params.push(format!("WAREHOUSE '{}'", esc(w)));
    }
    if let Some(r) = role {
        params.push(format!("ROLE '{}'", esc(r)));
    }
    let secret = secret_name(alias);
    format!(
        "CREATE OR REPLACE SECRET \"{sq}\" ({params}); \
         ATTACH IF NOT EXISTS '' AS \"{aq}\" \
         (TYPE snowflake, SECRET \"{sq}\", READ_ONLY, enable_pushdown true);",
        sq = quote(&secret),
        aq = quote(alias),
        params = params.join(", ")
    )
}

/// Validate + fold + quote a `schema.table` path, resolving it under the
/// attach alias. Parts are folded to UPPERCASE **before** quoting — the
/// double quotes are injection safety, and without the fold they would flip
/// the identifiers into Snowflake's case-*sensitive* resolution and stop
/// `raw.vehicle_models` from matching the real `RAW.VEHICLE_MODELS`
/// (Snowflake's own rule: unquoted identifiers fold to uppercase).
pub(super) fn qualify(alias: &str, table: &str) -> Result<String> {
    let (schema, tbl) = split_schema_table(table)?;
    Ok(format!(
        "\"{}\".\"{}\".\"{}\"",
        quote(alias),
        schema.to_ascii_uppercase(),
        tbl.to_ascii_uppercase()
    ))
}

/// `schema.table` — exactly two non-empty dot-separated parts; the database
/// comes from the connection, never a third part. A double quote anywhere is
/// rejected rather than escaped: datamk resolves the whole path with
/// Snowflake's unquoted-identifier (uppercase-fold) semantics, so quoting
/// can only be an attempt at a case-sensitive name — which the extension
/// cannot reach through the attach at all; `query:` is the documented route.
fn split_schema_table(table: &str) -> Result<(&str, &str)> {
    if table.contains('"') {
        bail!(
            "snowflake source table '{table}' contains a double quote — datamk resolves \
             `table:` paths with Snowflake's unquoted-identifier rule (folded to UPPERCASE), \
             and a case-sensitive (quoted) object is not reachable through `table:`; read it \
             with a `query:` source using quoted identifiers instead."
        );
    }
    match table.split('.').collect::<Vec<_>>().as_slice() {
        [schema, tbl] if !schema.is_empty() && !tbl.is_empty() => Ok((schema, tbl)),
        _ => bail!(
            "snowflake source table must be `schema.table`, got '{table}' \
             (the database comes from the connection; a cross-database read \
             is a second connection, not a three-part name)"
        ),
    }
}

/// No metadata job: every Snowflake object routes through the staged read
/// (`ObjectKind::Query`), so classification would buy only a cosmetic
/// table-vs-view label at the cost of a warehouse round-trip per schema per
/// run. Table-path shape is still validated here (fail at bind, before
/// ATTACH traffic); existence is checked by the staging read itself, whose
/// failure `rewrite_stage_error` turns actionable.
pub(super) fn classify_objects(tables: &[&str]) -> Result<IndexMap<String, ObjectMeta>> {
    let mut out = IndexMap::new();
    for &t in tables {
        split_schema_table(t)?;
        out.insert(
            t.to_string(),
            ObjectMeta {
                kind: ObjectKind::Query,
                columns: IndexMap::new(),
            },
        );
    }
    Ok(out)
}

/// The SELECT the engine wraps in `CREATE TEMP TABLE … AS` — through the
/// attach, with the watermark predicate (if any) rendered DuckDB-side
/// (`MarkValue::as_literal`); the extension pushes the comparison down to
/// Snowflake (live-verified byte-correct for DATE and TIMESTAMPTZ cursors).
/// Kind-independent: tables and views read through the same path.
pub(super) fn read_sql(
    alias: &str,
    table: &str,
    predicate: Option<&CursorPredicate>,
) -> Result<String> {
    let qualified = qualify(alias, table)?;
    Ok(match predicate {
        Some(p) => {
            let cq = p.cursor.replace('"', "\"\"");
            format!(
                "SELECT * FROM {qualified} WHERE \"{cq}\" > {}",
                p.mark.as_literal()
            )
        }
        None => format!("SELECT * FROM {qualified}"),
    })
}

/// ADR 0007 §2's server-side read for an author-owned `query:` source — the
/// extension's `snowflake_query()` table function over the connection's
/// secret. The connector's only transformation of `query` is `esc()` for
/// delivery: no identifier rewriting, no predicate injection. Unqualified
/// `schema.table` names in the query resolve against the connection's
/// `database` (the session database), so no `${connection.*}` binding
/// applies.
pub(super) fn query_read_sql(alias: &str, query: &str) -> String {
    format!(
        "SELECT * FROM snowflake_query('{}', '{}')",
        esc(query),
        esc(&secret_name(alias))
    )
}

/// A LOAD/ATTACH failure whose text names the extension's missing-ADBC-driver
/// lookup, rewritten into the actionable message (what the driver is, why
/// datamk can't fetch it, the env-var override, where the install command
/// lives); anything else is wrapped with the same context text every other
/// connector's attach failure gets. `passphrase` is the connection's key
/// passphrase, if any — the attach batch embeds it as a SQL literal, so any
/// error text that happens to echo the failing statement is scrubbed before
/// it can reach a log (defense in depth alongside the `Redacted` wrapper).
pub(super) fn rewrite_attach_error(
    err: duckdb::Error,
    connection: &str,
    passphrase: Option<&str>,
) -> anyhow::Error {
    let mut msg = err.to_string();
    if let Some(p) = passphrase.filter(|p| !p.is_empty()) {
        msg = msg.replace(p, "<redacted>");
        msg = msg.replace(&esc(p), "<redacted>");
    }
    // Live-diagnosed: `authenticator: externalbrowser` on an account with no
    // SAML/SSO identity provider. The stack's gosnowflake defaults console
    // login OFF and the ADBC driver exposes no switch for it, so the driver
    // always takes the SAML path — which an IdP-less account refuses with
    // exactly this server-side error (Snowflake error 390190).
    if msg.contains("SAML Identity Provider account parameter") {
        return anyhow::anyhow!(
            "connection '{connection}' (snowflake): the account rejected browser-based auth \
             (Snowflake error 390190) — `authenticator: externalbrowser` needs a SAML/SSO \
             identity provider (Okta, AzureAD, ...) configured on the Snowflake account, and \
             this account appears to have none. Use key-pair auth (`private_key_path:`) \
             instead, or configure a SAML2 security integration on the account.\n\n{msg}"
        );
    }
    if msg.contains("ADBC Snowflake driver") && msg.contains("not found") {
        anyhow::anyhow!(
            "connection '{connection}' (snowflake): the `snowflake` DuckDB extension loaded, \
             but the Snowflake ADBC driver it reads through (libadbc_driver_snowflake) is not \
             installed — datamk cannot fetch it from DuckDB's extension registry.\n\
             Fix one of two ways:\n  \
             1. install the driver at one of the searched paths (listed below), or\n  \
             2. set SNOWFLAKE_ADBC_DRIVER_PATH to the library's full path.\n\
             The per-platform download command is in docs/guides/snowflake.md#adbc-driver.\n\n\
             {msg}"
        )
    } else {
        // Built from the scrubbed text, not the original `err`, so a
        // statement-echoing error can never carry the passphrase onward.
        anyhow::anyhow!("{msg}").context(format!("attaching connection '{connection}' (snowflake)"))
    }
}

/// A staging-read failure rewritten into the actionable message for the two
/// failure shapes a first hour actually hits — no active warehouse (the
/// classic Snowflake footgun when `warehouse:` is unset and the user has no
/// default) and table-not-found (where the uppercase fold must be explained,
/// since the name in the error may not be the name the author wrote).
/// Anything else is wrapped with plain context. Snowflake has no analog of
/// BigQuery's ~10GB response ceiling — results stream over Arrow — so there
/// is no size-limit shape to detect here.
pub(super) fn rewrite_stage_error(
    err: duckdb::Error,
    name: &str,
    describe: &str,
    database: &str,
    warehouse: Option<&str>,
    role: Option<&str>,
    context: &str,
) -> anyhow::Error {
    let msg = err.to_string();
    if msg.contains("No active warehouse selected") {
        let fix = match warehouse {
            Some(w) => format!(
                "The connection sets `warehouse: {w}` — check the connection's user/role can \
                 use it (GRANT USAGE ON WAREHOUSE {w} ...)."
            ),
            None => "The connection sets no `warehouse:` and the user has no default, so \
                     Snowflake has no compute to run the read. Set `warehouse:` on the \
                     connection."
                .to_string(),
        };
        return anyhow::anyhow!(
            "source '{name}' ({describe}): no active warehouse. {fix}\n\n{msg}"
        );
    }
    // Deliberately narrow: DuckDB's own catalog error for a missing table
    // through the attach ("Catalog Error: Table with name X does not
    // exist!", live-captured) — NOT a bare "does not exist" match, which
    // Snowflake's warehouse/schema/role errors ("Warehouse 'X' does not
    // exist or not authorized") would false-positive into a misleading
    // table-not-found diagnosis.
    if describe != "query" && msg.contains("Table with name") && msg.contains("does not exist") {
        let seen_by = match role {
            Some(r) => format!("role {r}"),
            None => "the connection's user".to_string(),
        };
        return anyhow::anyhow!(
            "source '{name}' ({describe}): table not found in database {database}. datamk \
             folds unquoted identifiers to UPPERCASE (Snowflake's own rule) — check the table \
             exists as {folded} and that {seen_by} can see it. A genuinely lower/mixed-case \
             object (one created quoted) is unreachable via `table:` — read it with a `query:` \
             source using quoted identifiers instead.\n\n{msg}",
            folded = describe.to_ascii_uppercase(),
        );
    }
    anyhow::Error::new(err).context(context.to_string())
}

/// The non-incremental staged-read narration (the `ObjectKind::Query` bind
/// arm). Snowflake stages *everything*, so unlike BigQuery this fires for
/// plain base tables too — the message owns that honestly: full read every
/// run, transform filters don't push down, and the two levers that bound it.
pub(super) fn stage_narration(name: &str, table: &str) -> String {
    format!(
        "source '{name}' ({table}) is staged from Snowflake in full every run — transform \
         filters do not push down to the warehouse. Add `incremental:` with a cursor to read \
         only new rows, or use a `query:` source to aggregate server-side."
    )
}

/// The watermarked staged-read narration (`stage_incremental`'s
/// `ObjectKind::Query` arm with a mark).
pub(super) fn stage_incremental_narration(name: &str, table: &str) -> String {
    format!(
        "source '{name}' ({table}): staged delta past watermark — the predicate is pushed \
         into the Snowflake read."
    )
}

/// The `query:` source narration (ADR 0007), sibling of the BigQuery
/// "executing server-side via the BigQuery jobs API" line.
pub(super) fn query_stage_narration(name: &str) -> String {
    format!("source '{name}' is a query source — executing server-side in Snowflake")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Redacted;
    use crate::engine::MarkValue;

    fn keypair() -> SnowflakeAuth {
        SnowflakeAuth::KeyPair {
            user: "SVC_USER".to_string(),
            private_key_path: "/keys/sf.p8".to_string(),
            passphrase: None,
        }
    }

    #[test]
    fn attach_sql_is_read_only_pushdown_enabled_and_secret_aliased() {
        assert_eq!(
            attach_sql(
                "MYORG-ACCT",
                "ANALYTICS",
                &keypair(),
                None,
                None,
                "__conn_wh"
            ),
            "CREATE OR REPLACE SECRET \"__conn_wh_secret\" (TYPE snowflake, \
             ACCOUNT 'MYORG-ACCT', DATABASE 'ANALYTICS', USER 'SVC_USER', \
             AUTH_TYPE 'key_pair', PRIVATE_KEY '/keys/sf.p8'); \
             ATTACH IF NOT EXISTS '' AS \"__conn_wh\" (TYPE snowflake, \
             SECRET \"__conn_wh_secret\", READ_ONLY, enable_pushdown true);"
        );
    }

    #[test]
    fn attach_sql_includes_warehouse_role_and_passphrase_when_set() {
        let auth = SnowflakeAuth::KeyPair {
            user: "SVC_USER".to_string(),
            private_key_path: "/keys/sf.p8".to_string(),
            passphrase: Some(Redacted("s3'cret".to_string())),
        };
        let sql = attach_sql("A", "D", &auth, Some("WH"), Some("ANALYST"), "__conn_wh");
        assert!(sql.contains("PRIVATE_KEY_PASSPHRASE 's3''cret'"), "{sql}");
        assert!(sql.contains("WAREHOUSE 'WH'"), "{sql}");
        assert!(sql.contains("ROLE 'ANALYST'"), "{sql}");
    }

    #[test]
    fn attach_sql_external_browser_omits_key_and_carries_user() {
        let sql = attach_sql(
            "A",
            "D",
            &SnowflakeAuth::ExternalBrowser {
                user: "me@example.com".to_string(),
            },
            None,
            None,
            "__conn_wh",
        );
        assert!(sql.contains("AUTH_TYPE 'externalbrowser'"), "{sql}");
        assert!(!sql.contains("PRIVATE_KEY"), "{sql}");
        assert!(sql.contains("USER 'me@example.com'"), "{sql}");
    }

    #[test]
    fn qualify_folds_to_uppercase_then_quotes() {
        assert_eq!(
            qualify("__conn_wh", "raw.vehicle_models").unwrap(),
            "\"__conn_wh\".\"RAW\".\"VEHICLE_MODELS\""
        );
        assert_eq!(
            qualify("__conn_wh", "RAW.VEHICLE_MODELS").unwrap(),
            "\"__conn_wh\".\"RAW\".\"VEHICLE_MODELS\""
        );
    }

    #[test]
    fn qualify_rejects_one_and_three_part_names() {
        for bad in ["accounts", "db.sales.accounts", "sales.", ".accounts", ""] {
            let err = qualify("__conn_wh", bad).unwrap_err().to_string();
            assert!(err.contains("schema.table"), "for '{bad}': {err}");
        }
    }

    #[test]
    fn qualify_rejects_quoted_case_sensitive_names_steering_to_query() {
        let err = qualify("__conn_wh", "\"sqlmesh\".\"_versions\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("double quote"), "{err}");
        assert!(err.contains("`query:`"), "{err}");
    }

    #[test]
    fn classify_objects_synthesizes_query_kind_without_a_metadata_job() {
        let out = classify_objects(&["raw.a", "marts.b"]).unwrap();
        assert_eq!(out.len(), 2);
        for meta in out.values() {
            assert_eq!(meta.kind, ObjectKind::Query);
            assert!(meta.columns.is_empty());
        }
    }

    #[test]
    fn classify_objects_still_validates_table_shape() {
        let err = classify_objects(&["justonepart"]).unwrap_err().to_string();
        assert!(err.contains("schema.table"), "{err}");
    }

    #[test]
    fn read_sql_plain_reads_through_the_attach() {
        assert_eq!(
            read_sql("__conn_wh", "raw.vehicle_models", None).unwrap(),
            "SELECT * FROM \"__conn_wh\".\"RAW\".\"VEHICLE_MODELS\""
        );
    }

    #[test]
    fn read_sql_with_predicate_renders_a_duckdb_literal() {
        let mark = MarkValue::Ts("2026-07-04 10:58:00+00".to_string());
        let pred = CursorPredicate {
            cursor: "updated_at",
            mark: &mark,
        };
        assert_eq!(
            read_sql("__conn_wh", "raw.events", Some(&pred)).unwrap(),
            "SELECT * FROM \"__conn_wh\".\"RAW\".\"EVENTS\" WHERE \"updated_at\" > \
             TIMESTAMPTZ '2026-07-04 10:58:00+00'"
        );
    }

    #[test]
    fn query_read_sql_delivers_the_author_query_via_snowflake_query() {
        assert_eq!(
            query_read_sql("__conn_wh", "SELECT 'it''s fine' AS x"),
            "SELECT * FROM snowflake_query('SELECT ''it''''s fine'' AS x', \
             '__conn_wh_secret')"
        );
    }

    fn duck_err(msg: &str) -> duckdb::Error {
        duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error {
                code: duckdb::ffi::ErrorCode::Unknown,
                extended_code: 0,
            },
            Some(msg.to_string()),
        )
    }

    /// Live reality check: the exact error the extension emits when the ADBC
    /// driver is absent.
    #[test]
    fn rewrite_attach_error_makes_the_missing_driver_actionable() {
        let e = duck_err(
            "IO Error: ADBC Snowflake driver (libadbc_driver_snowflake.so) not found. \
             Searched locations:\n  - /Users/x/.duckdb/extensions/v1.5.4/osx_arm64/...",
        );
        let err = rewrite_attach_error(e, "wh", None).to_string();
        assert!(err.contains("SNOWFLAKE_ADBC_DRIVER_PATH"), "{err}");
        assert!(
            err.contains("docs/guides/snowflake.md#adbc-driver"),
            "{err}"
        );
        assert!(err.contains("Searched locations"), "{err}");
        assert!(err.contains("connection 'wh'"), "{err}");
    }

    #[test]
    fn rewrite_attach_error_wraps_other_failures_with_context() {
        let e = duck_err("JWT token is invalid");
        let err = rewrite_attach_error(e, "wh", None);
        assert!(
            err.to_string().contains("attaching connection 'wh'"),
            "{err}"
        );
        assert!(format!("{err:#}").contains("JWT token"), "{err:#}");
    }

    /// Live reality check: the exact server-side error an IdP-less account
    /// returns for the externalbrowser flow (reproduced with a raw
    /// authenticator-request POST — nothing client-side can avoid it).
    #[test]
    fn rewrite_attach_error_explains_external_browser_on_an_idp_less_account() {
        let e = duck_err(
            "IO Error: Failed to initialize connection: [snowflake] 390190 (08004): There was \
             an error related to the SAML Identity Provider account parameter. Contact \
             Snowflake support.",
        );
        let err = rewrite_attach_error(e, "wh", None).to_string();
        assert!(err.contains("SAML/SSO identity provider"), "{err}");
        assert!(err.contains("private_key_path"), "{err}");
        assert!(err.contains("390190"), "{err}");
    }

    #[test]
    fn rewrite_attach_error_scrubs_the_passphrase_from_any_error_text() {
        // Defense in depth: if an attach failure ever echoes the failing
        // statement, the passphrase (plain or SQL-escaped) must not survive
        // into the error chain.
        let e = duck_err("Parser Error: near PRIVATE_KEY_PASSPHRASE 's3''cret' in ATTACH");
        let err = format!("{:#}", rewrite_attach_error(e, "wh", Some("s3'cret")));
        assert!(
            !err.contains("s3'cret") && !err.contains("s3''cret"),
            "{err}"
        );
        assert!(err.contains("<redacted>"), "{err}");
    }

    #[test]
    fn rewrite_stage_error_names_the_warehouse_fix() {
        let e = duck_err("No active warehouse selected in the current session");
        let err = rewrite_stage_error(e, "s", "raw.t", "DB", None, None, "ctx").to_string();
        assert!(err.contains("no active warehouse"), "{err}");
        assert!(err.contains("Set `warehouse:`"), "{err}");

        let e = duck_err("No active warehouse selected in the current session");
        let err = rewrite_stage_error(e, "s", "raw.t", "DB", Some("WH"), None, "ctx").to_string();
        assert!(err.contains("GRANT USAGE ON WAREHOUSE WH"), "{err}");
    }

    /// Live reality check: DuckDB's catalog error for a missing table
    /// through the attach.
    #[test]
    fn rewrite_stage_error_explains_the_uppercase_fold_on_not_found() {
        let e = duck_err("Catalog Error: Table with name NO_SUCH_TABLE does not exist!");
        let err = rewrite_stage_error(
            e,
            "models",
            "raw.no_such_table",
            "CAR_MANUFACTURING",
            None,
            Some("CAR_MANUFACTURING_ROLE"),
            "ctx",
        )
        .to_string();
        assert!(err.contains("RAW.NO_SUCH_TABLE"), "{err}");
        assert!(err.contains("UPPERCASE"), "{err}");
        assert!(err.contains("role CAR_MANUFACTURING_ROLE"), "{err}");
        assert!(err.contains("`query:`"), "{err}");
        // The root cause is appended, never discarded.
        assert!(err.contains("Catalog Error"), "{err}");
    }

    #[test]
    fn rewrite_stage_error_never_misreads_a_missing_warehouse_as_a_missing_table() {
        // Snowflake's own "does not exist or not authorized" wording for a
        // warehouse/schema/role must not trip the table-not-found rewrite —
        // the match is DuckDB's catalog-error shape ("Table with name"),
        // nothing looser.
        let e =
            duck_err("IO Error: [snowflake] Warehouse 'TYPO_WH' does not exist or not authorized.");
        let err = rewrite_stage_error(e, "s", "raw.t", "DB", Some("TYPO_WH"), None, "staging ctx");
        let msg = format!("{err:#}");
        assert!(!msg.contains("table not found"), "{msg}");
        assert!(msg.contains("TYPO_WH"), "{msg}");
        assert!(msg.contains("staging ctx"), "{msg}");
    }

    #[test]
    fn rewrite_stage_error_never_misreads_a_query_source_as_not_found() {
        // A `query:` source's failure text can legitimately contain "does
        // not exist" (Snowflake's own compilation error) — the not-found
        // rewrite is table-shaped and must not fire for `describe == "query"`.
        let e = duck_err("SQL compilation error: Object 'X' does not exist or not authorized");
        let err = rewrite_stage_error(e, "s", "query", "DB", None, None, "staging ctx");
        assert!(err.to_string().contains("staging ctx"), "{err}");
    }

    #[test]
    fn rewrite_stage_error_wraps_other_failures_with_context() {
        let e = duck_err("Connection reset by peer");
        let err = rewrite_stage_error(e, "s", "raw.t", "DB", None, None, "staging ctx");
        assert!(err.to_string().contains("staging ctx"), "{err}");
        assert!(format!("{err:#}").contains("Connection reset"), "{err:#}");
    }

    #[test]
    fn narrations_name_the_source_and_the_levers() {
        let n = stage_narration("hourly", "staging.hourly_production");
        assert!(n.contains("full every run"), "{n}");
        assert!(n.contains("`incremental:`"), "{n}");
        assert!(n.contains("`query:`"), "{n}");
        let n = stage_incremental_narration("hourly", "staging.hourly_production");
        assert!(n.contains("watermark"), "{n}");
        let n = query_stage_narration("spend");
        assert!(n.contains("server-side in Snowflake"), "{n}");
    }
}
