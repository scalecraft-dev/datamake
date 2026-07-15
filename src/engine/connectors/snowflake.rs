//! All Snowflake-specific connector code: extension load, secret + probe,
//! table-path validation/folding/quoting, and error rewrites. Realized via
//! the DuckDB community `snowflake` extension's `snowflake_query()` table
//! function **only** — datamk-composed Snowflake SQL delivered verbatim,
//! results streamed back over the Arrow ADBC Snowflake driver. The
//! extension's attached-catalog surface is never used (ADR 0009 §1a): its
//! scan rebuilds DuckDB query fragments into Snowflake SQL, and that
//! translation layer emits invalid SQL for zero-column projections (a bare
//! `SELECT COUNT(*)` renders `SELECT  FROM …` — live-diagnosed, present
//! upstream), so the connection is never ATTACHed at all.
//!
//! Two Snowflake-shaped divergences from the BigQuery connector:
//!
//! - **Every read is staged.** No Snowflake source is ever bound as a
//!   read-through view (the attach-scan hazard above, §1). `classify_objects`
//!   synthesizes `ObjectKind::Query` for everything, with no metadata job:
//!   routing doesn't depend on the object's real kind, predicates render in
//!   Snowflake's own dialect inside the delivered SQL (no native column
//!   types needed), and the staging read itself is the not-found detector
//!   (`rewrite_stage_error`).
//! - **The ADBC driver is a separate native library** the extension loads at
//!   first use (`libadbc_driver_snowflake`) — not installable from DuckDB's
//!   extension registry, so a missing driver gets an actionable rewrite
//!   (`rewrite_attach_error`, fired by the setup probe) instead of the raw
//!   searched-paths error.

use anyhow::{bail, Result};
use indexmap::IndexMap;

use super::{quote, CursorPredicate, ObjectKind, ObjectMeta};
use crate::config::SnowflakeAuth;
use crate::engine::{esc, MarkValue};

pub(super) const INSTALL_LOAD_SQL: &str = "INSTALL snowflake FROM community; LOAD snowflake;";

/// The session-local DuckDB secret every `snowflake_query()` call
/// references, derived from the connection alias so `attach_sql`, `qualify`,
/// `read_sql`, and `query_read_sql` can never disagree on the name.
fn secret_name(alias: &str) -> String {
    format!("{alias}_secret")
}

/// Wrap datamk-composed Snowflake SQL as the extension's `snowflake_query()`
/// table function — the connector's single read mechanism (ADR 0009 §1a).
/// `esc()` at the wrapping boundary is the only escaping layer: the inner
/// SQL is composed plain, then delivered as one DuckDB string literal.
fn wrap_query(inner_sql: &str, alias: &str) -> String {
    format!(
        "snowflake_query('{}', '{}')",
        esc(inner_sql),
        esc(&secret_name(alias))
    )
}

/// The secret + connectivity-probe batch — there is no ATTACH (ADR 0009
/// §1a). `CREATE OR REPLACE SECRET` keyed on the connection name is
/// idempotent (a local, session-scoped metadata write with identical
/// values). The `SELECT 1` probe forces ADBC driver load and authentication
/// at the same lifecycle point ATTACH used to fail at, so missing-driver /
/// bad-key / SSO errors keep their setup-time rewrites — and it runs even
/// with no active warehouse (live-verified).
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
         SELECT * FROM {probe};",
        sq = quote(&secret),
        params = params.join(", "),
        probe = wrap_query("SELECT 1", alias)
    )
}

/// Validate + fold + quote the *Snowflake-side* three-part path
/// (`"DB"."SCHEMA"."TABLE"`, database from the connection — explicit rather
/// than session-resolved). Parts are folded to UPPERCASE **before** quoting
/// — the double quotes are injection safety, and without the fold they
/// would flip the identifiers into Snowflake's case-*sensitive* resolution
/// and stop `raw.vehicle_models` from matching the real `RAW.VEHICLE_MODELS`
/// (Snowflake's own rule: unquoted identifiers fold to uppercase).
fn snowflake_path(database: &str, table: &str) -> Result<String> {
    if database.contains('"') {
        bail!(
            "snowflake connection `database: {database}` contains a double quote — datamk \
             resolves it with Snowflake's unquoted-identifier rule (folded to UPPERCASE); a \
             case-sensitive (quoted) database name is not supported."
        );
    }
    let (schema, tbl) = split_schema_table(table)?;
    Ok(format!(
        "\"{}\".\"{}\".\"{}\"",
        database.to_ascii_uppercase(),
        schema.to_ascii_uppercase(),
        tbl.to_ascii_uppercase()
    ))
}

/// The DuckDB-side relation expression for a `table:` source — the
/// `snowflake_query()` wrapper around a plain `SELECT *` of the three-part
/// path. The engine consumes this in exactly one place for Snowflake:
/// `DESCRIBE SELECT * FROM <qualified>` (incremental cursor validation),
/// which binds via the extension's `LIMIT 0` schema probe — one cheap
/// server round-trip, real column names and types (live-verified).
pub(super) fn qualify(alias: &str, database: &str, table: &str) -> Result<String> {
    let path = snowflake_path(database, table)?;
    Ok(wrap_query(&format!("SELECT * FROM {path}"), alias))
}

/// `schema.table` — exactly two non-empty dot-separated parts; the database
/// comes from the connection, never a third part. A double quote anywhere is
/// rejected rather than escaped: datamk resolves the whole path with
/// Snowflake's unquoted-identifier (uppercase-fold) semantics, so quoting
/// can only be an attempt at a case-sensitive name — deliberately not
/// addressable via `table:`; `query:` is the documented route.
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
/// run. Table-path shape is still validated here (fail at bind, before any
/// read traffic); existence is checked by the staging read itself, whose
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

/// Render the watermark as a Snowflake-dialect literal for the server-side
/// predicate — connector-owned, mirroring the BigQuery pattern (`MarkValue`
/// itself grows no per-connector rendering mode). The cast form
/// (`'…'::TIMESTAMP_TZ`) rather than a typed-literal keyword: Snowflake has
/// no `TIMESTAMPTZ '…'` syntax; both cast forms are live-verified. `esc()`
/// on the string variants is defense in depth against a crafted value
/// escaping its literal *within the inner SQL* (the whole inner SQL is
/// additionally escaped once at the `wrap_query` boundary — two independent
/// layers, each correct alone).
fn snowflake_literal(mark: &MarkValue) -> String {
    match mark {
        MarkValue::Ts(s) => format!("'{}'::TIMESTAMP_TZ", esc(s)),
        MarkValue::Date(s) => format!("'{}'::DATE", esc(s)),
        MarkValue::Int(n) => n.to_string(),
    }
}

/// The SELECT the engine wraps in `CREATE TEMP TABLE … AS` — via
/// `snowflake_query()`, with the watermark predicate (if any) baked into
/// the server-side SQL in Snowflake's own dialect
/// (`MarkValue::as_snowflake_literal` — live-verified byte-correct for DATE
/// and TIMESTAMP_TZ cursors). The cursor identifier follows the §4 rule:
/// folded to UPPERCASE, then quoted (a created-quoted, case-sensitive
/// cursor column is not addressable — same boundary the table path draws).
/// Kind-independent: tables and views read through the same path.
pub(super) fn read_sql(
    alias: &str,
    database: &str,
    table: &str,
    predicate: Option<&CursorPredicate>,
) -> Result<String> {
    let path = snowflake_path(database, table)?;
    let inner = match predicate {
        Some(p) => {
            let cq = p.cursor.to_ascii_uppercase().replace('"', "\"\"");
            format!(
                "SELECT * FROM {path} WHERE \"{cq}\" > {}",
                snowflake_literal(p.mark)
            )
        }
        None => format!("SELECT * FROM {path}"),
    };
    Ok(format!("SELECT * FROM {}", wrap_query(&inner, alias)))
}

/// ADR 0007 §2's server-side read for an author-owned `query:` source — the
/// extension's `snowflake_query()` table function over the connection's
/// secret. The connector's only transformation of `query` is `esc()` for
/// delivery: no identifier rewriting, no predicate injection. Unqualified
/// `schema.table` names in the query resolve against the connection's
/// `database` (the session database), so no `${connection.*}` binding
/// applies.
pub(super) fn query_read_sql(alias: &str, query: &str) -> String {
    format!("SELECT * FROM {}", wrap_query(query, alias))
}

/// A LOAD/setup (secret + probe) failure whose text names the extension's
/// missing-ADBC-driver lookup, rewritten into the actionable message (what
/// the driver is, why datamk can't fetch it, the env-var override, where the
/// install command lives); anything else is wrapped with the same context
/// text every other connector's attach failure gets. `passphrase` is the
/// connection's key passphrase, if any — the setup batch embeds it as a SQL
/// literal, so any error text that happens to echo the failing statement is
/// scrubbed before it can reach a log (defense in depth alongside the
/// `Redacted` wrapper).
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
    // Deliberately narrow: Snowflake's own compilation error for a missing
    // object through `snowflake_query` ("SQL compilation error: Object
    // 'DB.SCHEMA.TABLE' does not exist or not authorized.", error 002003,
    // live-captured) — the `Object '` prefix is load-bearing, because
    // Snowflake's warehouse/schema/role errors ("Warehouse 'X' does not
    // exist or not authorized") share the suffix and would otherwise
    // false-positive into a misleading table-not-found diagnosis.
    if describe != "query"
        && msg.contains("SQL compilation error")
        && msg.contains("Object '")
        && msg.contains("does not exist or not authorized")
    {
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
        "source '{name}' ({table}): staged delta past watermark — the predicate executes \
         server-side in the Snowflake read."
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
    fn attach_sql_creates_secret_and_probes_without_attaching() {
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
             SELECT * FROM snowflake_query('SELECT 1', '__conn_wh_secret');"
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
    fn qualify_folds_to_uppercase_and_wraps_as_snowflake_query() {
        assert_eq!(
            qualify("__conn_wh", "analytics", "raw.vehicle_models").unwrap(),
            "snowflake_query('SELECT * FROM \"ANALYTICS\".\"RAW\".\"VEHICLE_MODELS\"', \
             '__conn_wh_secret')"
        );
        assert_eq!(
            qualify("__conn_wh", "ANALYTICS", "RAW.VEHICLE_MODELS").unwrap(),
            "snowflake_query('SELECT * FROM \"ANALYTICS\".\"RAW\".\"VEHICLE_MODELS\"', \
             '__conn_wh_secret')"
        );
    }

    #[test]
    fn qualify_rejects_one_and_three_part_names() {
        for bad in ["accounts", "db.sales.accounts", "sales.", ".accounts", ""] {
            let err = qualify("__conn_wh", "ANALYTICS", bad)
                .unwrap_err()
                .to_string();
            assert!(err.contains("schema.table"), "for '{bad}': {err}");
        }
    }

    #[test]
    fn qualify_rejects_quoted_case_sensitive_names_steering_to_query() {
        let err = qualify("__conn_wh", "ANALYTICS", "\"sqlmesh\".\"_versions\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("double quote"), "{err}");
        assert!(err.contains("`query:`"), "{err}");
    }

    #[test]
    fn qualify_rejects_a_quoted_database_name() {
        let err = qualify("__conn_wh", "\"analytics\"", "raw.t")
            .unwrap_err()
            .to_string();
        assert!(err.contains("database"), "{err}");
        assert!(err.contains("double quote"), "{err}");
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
    fn read_sql_plain_reads_via_snowflake_query() {
        assert_eq!(
            read_sql("__conn_wh", "ANALYTICS", "raw.vehicle_models", None).unwrap(),
            "SELECT * FROM snowflake_query('SELECT * FROM \
             \"ANALYTICS\".\"RAW\".\"VEHICLE_MODELS\"', '__conn_wh_secret')"
        );
    }

    #[test]
    fn read_sql_with_ts_predicate_renders_a_server_side_snowflake_literal() {
        let mark = MarkValue::Ts("2026-07-04 10:58:00+00".to_string());
        let pred = CursorPredicate {
            cursor: "updated_at",
            mark: &mark,
        };
        // The cursor folds to UPPERCASE (§4) and the literal is Snowflake
        // dialect; the inner SQL's single quotes are doubled once by the
        // `wrap_query` boundary.
        assert_eq!(
            read_sql("__conn_wh", "ANALYTICS", "raw.events", Some(&pred)).unwrap(),
            "SELECT * FROM snowflake_query('SELECT * FROM \"ANALYTICS\".\"RAW\".\"EVENTS\" \
             WHERE \"UPDATED_AT\" > ''2026-07-04 10:58:00+00''::TIMESTAMP_TZ', \
             '__conn_wh_secret')"
        );
    }

    #[test]
    fn read_sql_with_date_and_int_predicates_render_snowflake_dialect() {
        let mark = MarkValue::Date("2026-07-04".to_string());
        let pred = CursorPredicate {
            cursor: "d",
            mark: &mark,
        };
        let sql = read_sql("__conn_wh", "ANALYTICS", "raw.events", Some(&pred)).unwrap();
        assert!(sql.contains("\"D\" > ''2026-07-04''::DATE"), "{sql}");

        let mark = MarkValue::Int(42);
        let pred = CursorPredicate {
            cursor: "seq",
            mark: &mark,
        };
        let sql = read_sql("__conn_wh", "ANALYTICS", "raw.events", Some(&pred)).unwrap();
        assert!(sql.contains("\"SEQ\" > 42"), "{sql}");
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

    /// Live reality check: Snowflake's own compilation error for a missing
    /// object through `snowflake_query` (error 002003, captured 2026-07-15).
    #[test]
    fn rewrite_stage_error_explains_the_uppercase_fold_on_not_found() {
        let e = duck_err(
            "IO Error: Failed to execute snowflake_query: [snowflake] 002003 (42S02): \
             SQL compilation error:\nObject 'CAR_MANUFACTURING.RAW.NO_SUCH_TABLE' does not \
             exist or not authorized.",
        );
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
        assert!(err.contains("SQL compilation error"), "{err}");
    }

    #[test]
    fn rewrite_stage_error_never_misreads_a_missing_warehouse_as_a_missing_table() {
        // Snowflake's own "does not exist or not authorized" wording for a
        // warehouse/schema/role must not trip the table-not-found rewrite —
        // the match requires the compilation error's `Object '` prefix,
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
        // A `query:` source's failure text is the *same* compilation-error
        // shape a missing table produces (the author's SQL names its own
        // objects) — the not-found rewrite is table-shaped and must not fire
        // for `describe == "query"`, whose fold/`query:` guidance would be
        // nonsense for author-owned SQL.
        let e = duck_err(
            "IO Error: Failed to execute snowflake_query: [snowflake] 002003 (42S02): \
             SQL compilation error:\nObject 'X' does not exist or not authorized.",
        );
        let err = rewrite_stage_error(e, "s", "query", "DB", None, None, "staging ctx");
        let msg = format!("{err:#}");
        assert!(!msg.contains("table not found"), "{msg}");
        assert!(msg.contains("staging ctx"), "{msg}");
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
