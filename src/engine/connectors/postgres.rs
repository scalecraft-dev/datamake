//! All Postgres-specific connector code: extension load, secret + ATTACH,
//! table-path validation/quoting, and error rewrites. Realized via DuckDB's
//! **core** `postgres` extension (postgres_scanner) — the same extension the
//! engine already loads for metadata-DB catalogs, so unlike BigQuery and
//! Snowflake there is no community extension and no extra native driver.
//!
//! The Postgres-shaped divergences from the other two connectors (ADR 0010,
//! every claim live-verified against Postgres 16 through DuckDB 1.5.4):
//!
//! - **Every read is read-through.** postgres_scanner survives arbitrary
//!   transform SQL (`COUNT(*)`, joins, window functions — the shapes that
//!   disqualified Snowflake's attach-scan) over tables, views, and
//!   materialized views alike, with filter and projection pushdown into the
//!   scan. So `classify_objects` synthesizes `ObjectKind::Table` for
//!   everything, with no metadata job: sources bind as read-through TEMP
//!   VIEWs and transforms pull only the rows and columns they ask for —
//!   the OLTP-friendly posture (no full-table staging copy per run).
//! - **Build atomicity holds anyway:** within one DuckDB transaction the
//!   extension pins a single Postgres snapshot across statements
//!   (live-verified against a table mutating ~20×/second), so N transforms
//!   reading through see one consistent state of the source.
//! - **Auth is username+password over libpq** — the exact literal-secret
//!   shape ADR 0003 §2 keeps out of profiles, so the password is pure-`${VAR}`
//!   or ambient (`resolve_postgres`), delivered via a session-local
//!   `CREATE SECRET`, and scrubbed from any attach error text. Never a DSN:
//!   a libpq keyword string that fails to parse echoes the password back in
//!   the error (live-verified).

use anyhow::{bail, Result};
use indexmap::IndexMap;

use super::{quote, CursorPredicate, ObjectKind, ObjectMeta};
use crate::engine::esc;

/// Core extension — no `FROM community`, and already baked wherever the
/// metadata-DB catalog path works.
pub(super) const INSTALL_LOAD_SQL: &str = "INSTALL postgres; LOAD postgres;";

/// The session-local DuckDB secret a connection's ATTACH references, derived
/// from the attach alias (same convention as Snowflake).
fn secret_name(alias: &str) -> String {
    format!("{alias}_secret")
}

/// The secret + ATTACH batch. `CREATE OR REPLACE SECRET` + `ATTACH IF NOT
/// EXISTS` keyed on the connection name: a connection shared by several
/// sources attaches once, and re-running the batch is idempotent. The
/// password (if any) travels only inside the secret — the ATTACH path string
/// carries just `sslmode`, whose value is resolve-time-validated against
/// libpq's closed set so it needs no escaping. With no `PASSWORD`, libpq's
/// ambient chain applies (`PGPASSWORD`, `~/.pgpass`).
pub(super) fn attach_sql(
    host: &str,
    port: u16,
    database: &str,
    user: &str,
    password: Option<&str>,
    sslmode: &str,
    alias: &str,
) -> String {
    let mut params = vec![
        "TYPE postgres".to_string(),
        format!("HOST '{}'", esc(host)),
        format!("PORT {port}"),
        format!("DATABASE '{}'", esc(database)),
        format!("USER '{}'", esc(user)),
    ];
    if let Some(p) = password {
        params.push(format!("PASSWORD '{}'", esc(p)));
    }
    let secret = secret_name(alias);
    format!(
        "CREATE OR REPLACE SECRET \"{sq}\" ({params}); \
         ATTACH IF NOT EXISTS 'sslmode={sslmode}' AS \"{aq}\" \
         (TYPE postgres, SECRET \"{sq}\", READ_ONLY);",
        sq = quote(&secret),
        aq = quote(alias),
        params = params.join(", ")
    )
}

/// Validate + quote a `schema.table` path, resolving it under the attach
/// alias. Parts are quoted **verbatim** — no case fold: DuckDB resolves
/// identifiers against the attached Postgres catalog case-insensitively in
/// every direction (live-verified: `SALES.ORDERS`, unquoted `CamelTable`,
/// and quoted lowercase all reach their tables), so unlike Snowflake there
/// is no fold rule for the author to trip over via `table:`. Postgres's own
/// lowercase fold only matters inside a `query:` body, whose SQL runs on the
/// server verbatim.
pub(super) fn qualify(alias: &str, table: &str) -> Result<String> {
    let (schema, tbl) = split_schema_table(table)?;
    Ok(format!(
        "\"{}\".\"{}\".\"{}\"",
        quote(alias),
        quote(schema),
        quote(tbl)
    ))
}

/// `schema.table` — exactly two non-empty dot-separated parts; the database
/// comes from the connection, never a third part. No `public` default for a
/// bare name: BigQuery and Snowflake both require two parts, and one path
/// grammar across connectors beats saving six characters. A double quote is
/// rejected rather than escaped — the resolution is case-insensitive, so
/// quoting can only be an operator error worth surfacing loudly.
fn split_schema_table(table: &str) -> Result<(&str, &str)> {
    if table.contains('"') {
        bail!(
            "postgres source table '{table}' contains a double quote — `table:` paths resolve \
             case-insensitively, so quoting is never needed here; write the bare \
             `schema.table` (e.g. `public.orders`)."
        );
    }
    match table.split('.').collect::<Vec<_>>().as_slice() {
        [schema, tbl] if !schema.is_empty() && !tbl.is_empty() => Ok((schema, tbl)),
        _ => bail!(
            "postgres source table must be `schema.table`, got '{table}' (Postgres's default \
             schema is usually 'public', so a bare table name is written `public.{table}`; \
             the database comes from the connection, never a third part)"
        ),
    }
}

/// No metadata job, and no staging either: postgres_scanner reads tables,
/// views, and materialized views through one robust attach-scan with
/// pushdown (live-verified — including the `COUNT(*)` shape that forced
/// Snowflake to stage everything), so every object is `ObjectKind::Table`
/// and binds as a read-through TEMP VIEW. Table-path shape is still
/// validated here (fail at bind, before ATTACH traffic); existence is
/// checked by the first read, whose failure `rewrite_stage_error` turns
/// actionable. Empty `columns`: predicates render DuckDB-side (DuckDB's
/// type system descends from Postgres's — no native-type disambiguation
/// needed, unlike BigQuery's TIMESTAMP/DATETIME collapse).
pub(super) fn classify_objects(tables: &[&str]) -> Result<IndexMap<String, ObjectMeta>> {
    let mut out = IndexMap::new();
    for &t in tables {
        split_schema_table(t)?;
        out.insert(
            t.to_string(),
            ObjectMeta {
                kind: ObjectKind::Table,
                columns: IndexMap::new(),
            },
        );
    }
    Ok(out)
}

/// The SELECT for a staged read. Defensive: with everything classified
/// `ObjectKind::Table`, the engine never routes a Postgres source through
/// this — but a connector must still answer coherently rather than panic if
/// a future routing change reaches it. Same shape as the engine's own
/// `Table`-kind staging SQL: through the attach, predicate rendered
/// DuckDB-side (pushdown carries it into the Postgres scan, live-verified).
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
/// extension's `postgres_query()` table function over the attached database.
/// The connector's only transformation of `query` is `esc()` for delivery:
/// no identifier rewriting, no predicate injection. The query runs on the
/// server verbatim, so Postgres's own rules apply inside it: unqualified
/// names resolve via `search_path`, unquoted identifiers fold to lowercase,
/// and a case-sensitive (quoted-created) object is reached with quoted
/// identifiers.
pub(super) fn query_read_sql(alias: &str, query: &str) -> String {
    format!(
        "SELECT * FROM postgres_query('{}', '{}')",
        esc(alias),
        esc(query)
    )
}

/// A LOAD/ATTACH failure rewritten into the actionable shape for the four
/// first-hour failures (all live-captured against Postgres 16): auth
/// failure, connection refused, database-does-not-exist, and the two SSL
/// mismatches. `password` is the connection's password, if any — the attach
/// batch embeds it in the `CREATE SECRET` literal, so any error text that
/// happens to echo the failing statement is scrubbed before it can reach a
/// log (defense in depth alongside the `Redacted` wrapper).
#[allow(clippy::too_many_arguments)]
pub(super) fn rewrite_attach_error(
    err: duckdb::Error,
    connection: &str,
    host: &str,
    port: u16,
    database: &str,
    user: &str,
    sslmode: &str,
    password: Option<&str>,
) -> anyhow::Error {
    let mut msg = err.to_string();
    if let Some(p) = password.filter(|p| !p.is_empty()) {
        msg = msg.replace(&esc(p), "<redacted>");
        msg = msg.replace(p, "<redacted>");
    }
    // Live-captured: `FATAL: password authentication failed for user "x"`.
    if msg.contains("password authentication failed") {
        return anyhow::anyhow!(
            "connection '{connection}' (postgres): password authentication failed for user \
             '{user}'. Check the value behind `password:` (or the ambient PGPASSWORD/~/.pgpass \
             chain if it's unset), and that '{user}' can log in to database '{database}'.\n\n\
             {msg}"
        );
    }
    // Live-captured: `connection to server at "h" (ip), port p failed:
    // Connection refused`.
    if msg.contains("Connection refused") {
        return anyhow::anyhow!(
            "connection '{connection}' (postgres): could not connect to {host}:{port} \
             (connection refused). Check `host:`/`port:` in the profile and that the server \
             is reachable from here (VPN? network egress from the pod?).\n\n{msg}"
        );
    }
    // Live-captured: `FATAL: database "x" does not exist`.
    if msg.contains("database") && msg.contains("does not exist") {
        return anyhow::anyhow!(
            "connection '{connection}' (postgres): database '{database}' does not exist on \
             {host}:{port}. Check `database:` in the profile.\n\n{msg}"
        );
    }
    // Live-captured: `server does not support SSL, but SSL was required`
    // (datamk defaults sslmode=require). The reverse shape — the server
    // *demands* SSL from a `sslmode: disable` client — arrives as a
    // pg_hba.conf rejection naming "no encryption".
    if msg.contains("SSL") && (msg.contains("required") || msg.contains("no encryption")) {
        return anyhow::anyhow!(
            "connection '{connection}' (postgres): TLS mismatch with {host}:{port} (the \
             connection uses `sslmode: {sslmode}`; datamk defaults to `require`, unlike \
             libpq's `prefer`). For a local/trusted server without TLS set `sslmode: disable` \
             on the connection; for a remote one, the server needs TLS configured.\n\n{msg}"
        );
    }
    // Built from the scrubbed text, not the original `err`, so a
    // statement-echoing error can never carry the password onward.
    anyhow::anyhow!("{msg}").context(format!("attaching connection '{connection}' (postgres)"))
}

/// A read failure rewritten into the actionable shape for the two
/// first-hour failures past attach (both live-captured): table-not-found
/// (DuckDB's catalog error through the attach) and permission-denied
/// (surfaced from the scan's server-side COPY). Anything else is wrapped
/// with plain context. Postgres has no analog of BigQuery's ~10GB response
/// ceiling — reads stream over the wire — so there is no size-limit shape
/// to detect here.
pub(super) fn rewrite_stage_error(
    err: duckdb::Error,
    name: &str,
    describe: &str,
    database: &str,
    user: &str,
    context: &str,
) -> anyhow::Error {
    let msg = err.to_string();
    // Live-captured: DuckDB's own catalog error for a missing table through
    // the attach ("Catalog Error: Table with name X does not exist!").
    // Deliberately narrow, like the Snowflake arm — never a bare "does not
    // exist" match.
    if describe != "query" && msg.contains("Table with name") && msg.contains("does not exist") {
        return anyhow::anyhow!(
            "source '{name}' ({describe}): table not found in database '{database}'. Check it \
             exists and that '{user}' has USAGE on its schema. `table:` paths resolve \
             case-insensitively, so case is not the problem; a name with a dot or other \
             special characters is reachable via a `query:` source with quoted \
             identifiers.\n\n{msg}"
        );
    }
    // Live-captured: `ERROR: permission denied for table x` (surfaced
    // through the scan's server-side COPY statement).
    if msg.contains("permission denied for") {
        return anyhow::anyhow!(
            "source '{name}' ({describe}): permission denied — '{user}' needs SELECT. On the \
             server: GRANT USAGE ON SCHEMA <schema> TO {user}; GRANT SELECT ON <schema>.<table> \
             TO {user};\n\n{msg}"
        );
    }
    // From a `query:` body run server-side: `ERROR: relation "x" does not
    // exist` — here Postgres's lowercase fold IS in play, because the
    // author's SQL runs verbatim.
    if describe == "query" && msg.contains("relation") && msg.contains("does not exist") {
        return anyhow::anyhow!(
            "source '{name}' (query): a relation in the query does not exist. The query runs \
             on the Postgres server verbatim: unqualified names resolve via search_path, and \
             unquoted identifiers fold to lowercase — a case-sensitive (quoted-created) name \
             needs double quotes inside the query.\n\n{msg}"
        );
    }
    anyhow::Error::new(err).context(context.to_string())
}

/// The staged-read narrations. Unreachable in the shipped routing —
/// everything classifies `ObjectKind::Table` and binds read-through — but a
/// narration must answer coherently, not panic, if a future routing change
/// reaches it.
pub(super) fn stage_narration(name: &str, table: &str) -> String {
    format!("source '{name}' ({table}) is staged from Postgres in full this run.")
}

/// Sibling of `stage_narration`, for a watermarked delta.
pub(super) fn stage_incremental_narration(name: &str, table: &str) -> String {
    format!(
        "source '{name}' ({table}): staged delta past watermark — the predicate is pushed \
         into the Postgres read."
    )
}

/// The `query:` source narration (ADR 0007).
pub(super) fn query_stage_narration(name: &str) -> String {
    format!("source '{name}' is a query source — executing server-side in Postgres")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::MarkValue;

    #[test]
    fn attach_sql_is_read_only_and_secret_aliased_with_sslmode_in_the_path() {
        assert_eq!(
            attach_sql(
                "db.internal",
                5432,
                "analytics",
                "datamk_ro",
                None,
                "require",
                "__conn_pg"
            ),
            "CREATE OR REPLACE SECRET \"__conn_pg_secret\" (TYPE postgres, \
             HOST 'db.internal', PORT 5432, DATABASE 'analytics', USER 'datamk_ro'); \
             ATTACH IF NOT EXISTS 'sslmode=require' AS \"__conn_pg\" (TYPE postgres, \
             SECRET \"__conn_pg_secret\", READ_ONLY);"
        );
    }

    #[test]
    fn attach_sql_escapes_a_hostile_password_into_the_secret() {
        // Live-verified: '' doubling delivers `p'a''ss$ w{}rd%` intact
        // through CREATE SECRET, where the DSN form both fails to parse and
        // echoes the password back in the error.
        let sql = attach_sql(
            "h",
            5432,
            "d",
            "quotey",
            Some("p'a''ss$ w{}rd%"),
            "require",
            "__conn_pg",
        );
        assert!(sql.contains("PASSWORD 'p''a''''ss$ w{}rd%'"), "{sql}");
        assert!(!sql.contains("PASSWORD 'p'a"), "{sql}");
    }

    #[test]
    fn attach_sql_omits_password_for_the_ambient_chain() {
        let sql = attach_sql("h", 5432, "d", "u", None, "disable", "__conn_pg");
        assert!(!sql.contains("PASSWORD"), "{sql}");
        assert!(sql.contains("'sslmode=disable'"), "{sql}");
    }

    #[test]
    fn qualify_quotes_verbatim_without_folding() {
        assert_eq!(
            qualify("__conn_pg", "sales.orders").unwrap(),
            "\"__conn_pg\".\"sales\".\"orders\""
        );
        // No fold in either direction — DuckDB's attach lookup is
        // case-insensitive (live-verified), so the author's spelling is
        // preserved and still resolves.
        assert_eq!(
            qualify("__conn_pg", "Sales.Orders").unwrap(),
            "\"__conn_pg\".\"Sales\".\"Orders\""
        );
    }

    #[test]
    fn qualify_rejects_one_and_three_part_names_teaching_public() {
        for bad in ["orders", "db.sales.orders", "sales.", ".orders", ""] {
            let err = qualify("__conn_pg", bad).unwrap_err().to_string();
            assert!(err.contains("schema.table"), "for '{bad}': {err}");
        }
        let err = qualify("__conn_pg", "orders").unwrap_err().to_string();
        assert!(err.contains("public.orders"), "{err}");
    }

    #[test]
    fn qualify_rejects_quoted_names() {
        let err = qualify("__conn_pg", "\"public\".\"Orders\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("double quote"), "{err}");
        assert!(err.contains("case-insensitively"), "{err}");
    }

    #[test]
    fn classify_objects_synthesizes_table_kind_without_a_metadata_job() {
        // THE ADR 0010 decision: read-through for everything.
        let out = classify_objects(&["public.a", "sales.b"]).unwrap();
        assert_eq!(out.len(), 2);
        for meta in out.values() {
            assert_eq!(meta.kind, ObjectKind::Table);
            assert!(meta.columns.is_empty());
        }
    }

    #[test]
    fn classify_objects_still_validates_table_shape() {
        let err = classify_objects(&["justonepart"]).unwrap_err().to_string();
        assert!(err.contains("schema.table"), "{err}");
    }

    #[test]
    fn read_sql_reads_through_the_attach_with_a_duckdb_literal_predicate() {
        assert_eq!(
            read_sql("__conn_pg", "sales.orders", None).unwrap(),
            "SELECT * FROM \"__conn_pg\".\"sales\".\"orders\""
        );
        let mark = MarkValue::Ts("2026-07-04 10:58:00+00".to_string());
        let pred = CursorPredicate {
            cursor: "updated_at",
            mark: &mark,
        };
        assert_eq!(
            read_sql("__conn_pg", "sales.orders", Some(&pred)).unwrap(),
            "SELECT * FROM \"__conn_pg\".\"sales\".\"orders\" WHERE \"updated_at\" > \
             TIMESTAMPTZ '2026-07-04 10:58:00+00'"
        );
    }

    #[test]
    fn query_read_sql_delivers_the_author_query_via_postgres_query() {
        // Live-verified signature: postgres_query(<attached db>, <query>).
        assert_eq!(
            query_read_sql("__conn_pg", "SELECT 'it''s fine' AS x"),
            "SELECT * FROM postgres_query('__conn_pg', 'SELECT ''it''''s fine'' AS x')"
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

    fn rewrite_attach(msg: &str, password: Option<&str>) -> String {
        format!(
            "{:#}",
            rewrite_attach_error(
                duck_err(msg),
                "pg",
                "db.internal",
                5432,
                "analytics",
                "datamk_ro",
                "require",
                password,
            )
        )
    }

    /// Live reality check: libpq's auth-failure text through the extension.
    #[test]
    fn rewrite_attach_error_makes_auth_failure_actionable() {
        let err = rewrite_attach(
            "IO Error: Unable to connect to Postgres at \"\": connection to server at \
             \"localhost\" (127.0.0.1), port 5432 failed: FATAL:  password authentication \
             failed for user \"datamk_ro\"",
            None,
        );
        assert!(err.contains("connection 'pg'"), "{err}");
        assert!(err.contains("`password:`"), "{err}");
        assert!(err.contains("PGPASSWORD"), "{err}");
    }

    /// Live reality check: connection-refused, both address families.
    #[test]
    fn rewrite_attach_error_makes_connection_refused_actionable() {
        let err = rewrite_attach(
            "IO Error: Unable to connect to Postgres at \"\": connection to server at \
             \"localhost\" (127.0.0.1), port 54330 failed: Connection refused",
            None,
        );
        assert!(err.contains("db.internal:5432"), "{err}");
        assert!(err.contains("`host:`/`port:`"), "{err}");
    }

    /// Live reality check: `FATAL: database "nope" does not exist`.
    #[test]
    fn rewrite_attach_error_makes_a_missing_database_actionable() {
        let err = rewrite_attach(
            "IO Error: Unable to connect to Postgres at \"\": connection to server at \
             \"localhost\" (127.0.0.1), port 5432 failed: FATAL:  database \"nope\" does not \
             exist",
            None,
        );
        assert!(err.contains("database 'analytics'"), "{err}");
        assert!(err.contains("`database:`"), "{err}");
    }

    /// Live reality check: the sslmode=require-vs-plaintext-server shape —
    /// the first-hour failure datamk's require default makes reachable.
    #[test]
    fn rewrite_attach_error_explains_the_ssl_mismatch_and_the_require_default() {
        let err = rewrite_attach(
            "IO Error: Unable to connect to Postgres at \"sslmode=require\": connection to \
             server at \"localhost\" (127.0.0.1), port 5432 failed: server does not support \
             SSL, but SSL was required",
            None,
        );
        assert!(err.contains("sslmode: require"), "{err}");
        assert!(err.contains("`sslmode: disable`"), "{err}");
        assert!(err.contains("libpq's `prefer`"), "{err}");
    }

    #[test]
    fn rewrite_attach_error_scrubs_the_password_from_any_error_text() {
        // Defense in depth: if an attach failure ever echoes the failing
        // statement, the password (plain or SQL-escaped) must not survive
        // into the error chain.
        let err = rewrite_attach(
            "Parser Error: near PASSWORD 's3''cret' in CREATE SECRET",
            Some("s3'cret"),
        );
        assert!(
            !err.contains("s3'cret") && !err.contains("s3''cret"),
            "{err}"
        );
        assert!(err.contains("<redacted>"), "{err}");
    }

    #[test]
    fn rewrite_attach_error_wraps_other_failures_with_context() {
        let err = rewrite_attach("something unexpected", None);
        assert!(
            err.contains("attaching connection 'pg' (postgres)"),
            "{err}"
        );
        assert!(err.contains("something unexpected"), "{err}");
    }

    /// Live reality check: DuckDB's catalog error for a missing table
    /// through the attach.
    #[test]
    fn rewrite_stage_error_makes_table_not_found_actionable() {
        let e = duck_err("Catalog Error: Table with name no_such does not exist!");
        let err = rewrite_stage_error(
            e,
            "orders",
            "public.no_such",
            "analytics",
            "datamk_ro",
            "ctx",
        )
        .to_string();
        assert!(err.contains("database 'analytics'"), "{err}");
        assert!(err.contains("USAGE"), "{err}");
        assert!(err.contains("case-insensitively"), "{err}");
        // The root cause is appended, never discarded.
        assert!(err.contains("Catalog Error"), "{err}");
    }

    /// Live reality check: the scan surfaces the server's permission error
    /// through its COPY statement.
    #[test]
    fn rewrite_stage_error_makes_permission_denied_actionable() {
        let e = duck_err(
            "Invalid Error: Failed to prepare COPY \"COPY (SELECT NULL FROM \
             \"sales\".\"orders\" ...) TO STDOUT (FORMAT \"binary\");\": ERROR:  permission \
             denied for table orders",
        );
        let err = rewrite_stage_error(e, "orders", "sales.orders", "analytics", "datamk_ro", "ctx")
            .to_string();
        assert!(err.contains("GRANT SELECT"), "{err}");
        assert!(err.contains("GRANT USAGE ON SCHEMA"), "{err}");
        assert!(err.contains("datamk_ro"), "{err}");
    }

    #[test]
    fn rewrite_stage_error_explains_the_lowercase_fold_only_for_query_sources() {
        let e = duck_err("ERROR: relation \"CamelTable\" does not exist");
        let err = rewrite_stage_error(e, "spend", "query", "analytics", "u", "ctx").to_string();
        assert!(err.contains("lowercase"), "{err}");
        assert!(err.contains("search_path"), "{err}");
    }

    #[test]
    fn rewrite_stage_error_never_misreads_a_table_source_as_a_query_fold_problem() {
        // A table: source's not-found is DuckDB's catalog shape; the raw
        // server "relation does not exist" shape only reaches us from a
        // query: body, and the fold advice must not fire elsewhere.
        let e = duck_err("ERROR: relation \"x\" does not exist");
        let err = rewrite_stage_error(e, "s", "public.x", "d", "u", "staging ctx");
        let msg = format!("{err:#}");
        assert!(!msg.contains("lowercase"), "{msg}");
        assert!(msg.contains("staging ctx"), "{msg}");
    }

    #[test]
    fn rewrite_stage_error_wraps_other_failures_with_context() {
        let e = duck_err("Connection reset by peer");
        let err = rewrite_stage_error(e, "s", "public.t", "d", "u", "staging ctx");
        assert!(err.to_string().contains("staging ctx"), "{err}");
        assert!(format!("{err:#}").contains("Connection reset"), "{err:#}");
    }

    #[test]
    fn narrations_name_the_source() {
        assert!(stage_narration("orders", "public.orders").contains("orders"));
        assert!(stage_incremental_narration("orders", "public.orders").contains("watermark"));
        assert!(query_stage_narration("spend").contains("server-side in Postgres"));
    }
}
