//! Gated live-correctness test for the Postgres connector (ADR 0010): every
//! SQL shape the connector renders — the secret+ATTACH batch, read-through
//! binding, the incremental delta predicate, and the `postgres_query()`
//! passthrough — executed against a real Postgres through the same DuckDB
//! the crate bundles. Unlike BigQuery/Snowflake, this needs no cloud
//! account: any disposable Postgres works, e.g.
//!
//!   docker run -d --name datamk-pg-test -e POSTGRES_PASSWORD=testpass \
//!     -e POSTGRES_DB=datamk_test -p 54321:5432 postgres:16
//!   DATAMK_TEST_PG_HOST=localhost DATAMK_TEST_PG_PORT=54321 \
//!     DATAMK_TEST_PG_PASSWORD=testpass cargo test --test postgres_connector
//!
//! Convention (ADR 0003's "gated behind credentials, skipped when absent"):
//! lives in `tests/` (credentials-gating visible from the path alone) and
//! early-returns, with an `eprintln!` naming the skip, unless
//! `DATAMK_TEST_PG_HOST` is set. `cargo test` must stay green with no
//! server in the environment.
//!
//! This crate has no `[lib]` target (`datamk` is bin-only), so — like
//! `tests/bigquery_query_correctness.rs` — this test cannot `use` the
//! crate's internals. It executes the same SQL text the connector's
//! renderers are unit-tested to produce (`engine::connectors::postgres`),
//! which is the point: the unit tests pin the strings, this test proves the
//! strings actually run.

use duckdb::Connection;

/// Mirrors `engine::esc` (not reachable here — no `[lib]` target).
fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

struct PgEnv {
    host: String,
    port: String,
    database: String,
    user: String,
    password: String,
}

fn pg_env() -> Option<PgEnv> {
    let host = std::env::var("DATAMK_TEST_PG_HOST").ok()?;
    Some(PgEnv {
        host,
        port: std::env::var("DATAMK_TEST_PG_PORT").unwrap_or_else(|_| "5432".to_string()),
        database: std::env::var("DATAMK_TEST_PG_DB").unwrap_or_else(|_| "datamk_test".to_string()),
        user: std::env::var("DATAMK_TEST_PG_USER").unwrap_or_else(|_| "postgres".to_string()),
        password: std::env::var("DATAMK_TEST_PG_PASSWORD")
            .unwrap_or_else(|_| "testpass".to_string()),
    })
}

/// The exact batch `postgres::attach_sql` renders (pinned by its unit
/// tests), with the test server's coordinates. `sslmode=disable`: the
/// disposable test container has no TLS, and the explicit opt-out is
/// itself the documented shape for that.
fn attach_batch(pg: &PgEnv, alias: &str) -> String {
    format!(
        "CREATE OR REPLACE SECRET \"{alias}_secret\" (TYPE postgres, HOST '{}', PORT {}, \
         DATABASE '{}', USER '{}', PASSWORD '{}'); \
         ATTACH IF NOT EXISTS 'sslmode=disable' AS \"{alias}\" \
         (TYPE postgres, SECRET \"{alias}_secret\", READ_ONLY);",
        esc(&pg.host),
        pg.port,
        esc(&pg.database),
        esc(&pg.user),
        esc(&pg.password),
    )
}

#[test]
fn postgres_connector_shapes_run_live() {
    let Some(pg) = pg_env() else {
        eprintln!("skipping: DATAMK_TEST_PG_HOST not set (needs a disposable Postgres)");
        return;
    };

    let conn = Connection::open_in_memory().expect("duckdb");
    conn.execute_batch("INSTALL postgres; LOAD postgres;")
        .expect("core postgres extension");

    // Seed through a read-write attach + postgres_execute, so the test needs
    // no client-side psql at all.
    conn.execute_batch(&format!(
        "CREATE OR REPLACE SECRET rw_secret (TYPE postgres, HOST '{}', PORT {}, DATABASE '{}', \
         USER '{}', PASSWORD '{}'); \
         ATTACH IF NOT EXISTS 'sslmode=disable' AS rw (TYPE postgres, SECRET rw_secret);",
        esc(&pg.host),
        pg.port,
        esc(&pg.database),
        esc(&pg.user),
        esc(&pg.password),
    ))
    .expect("rw attach for seeding");
    let seed = "
        DROP SCHEMA IF EXISTS datamk_it CASCADE;
        CREATE SCHEMA datamk_it;
        CREATE TABLE datamk_it.orders (
            id bigint PRIMARY KEY,
            customer_id int NOT NULL,
            amount numeric(10,2),
            updated_at timestamptz NOT NULL
        );
        INSERT INTO datamk_it.orders
        SELECT g, (g % 10) + 1, g * 1.5,
               ''2026-01-01T00:00:00Z''::timestamptz + (g || '' hours'')::interval
        FROM generate_series(1, 1000) g;
        CREATE VIEW datamk_it.big_orders AS
            SELECT * FROM datamk_it.orders WHERE amount > 750;
    ";
    conn.execute_batch(&format!("CALL postgres_execute('rw', '{seed}');"))
        .expect("seeding fixture schema");

    // 1. The connector's attach batch runs, and re-running it is idempotent
    //    (CREATE OR REPLACE SECRET + ATTACH IF NOT EXISTS).
    conn.execute_batch(&attach_batch(&pg, "__conn_pg"))
        .expect("connector attach batch");
    conn.execute_batch(&attach_batch(&pg, "__conn_pg"))
        .expect("connector attach batch, second run");

    // 2. Read-through binding (the ObjectKind::Table bind arm, byte-shaped
    //    like `bind_source`), then the transform SQL that disqualified
    //    Snowflake's extension: bare COUNT(*), an aggregate+filter, a window.
    conn.execute_batch(
        "CREATE OR REPLACE TEMP VIEW \"orders\" AS SELECT * FROM \
         \"__conn_pg\".\"datamk_it\".\"orders\";",
    )
    .expect("read-through view bind");
    let n: i64 = conn
        .query_row("SELECT count(*) FROM \"orders\"", [], |r| r.get(0))
        .expect("COUNT(*) through the read-through view");
    assert_eq!(n, 1000);
    let (customers, total): (i64, f64) = conn
        .query_row(
            "SELECT count(DISTINCT customer_id), sum(amount)::DOUBLE FROM \"orders\" \
             WHERE amount > 300",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("aggregate through the read-through view");
    assert_eq!(customers, 10);
    assert!(total > 0.0);
    let top: i64 = conn
        .query_row(
            "SELECT id FROM (SELECT id, row_number() OVER (PARTITION BY customer_id \
             ORDER BY updated_at DESC) rn FROM \"orders\") WHERE rn = 1 ORDER BY id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("window function through the read-through view");
    assert!(top > 0);

    // 3. A view reads through the same path (no BigQuery-style split).
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM \"__conn_pg\".\"datamk_it\".\"big_orders\"",
            [],
            |r| r.get(0),
        )
        .expect("view through the read-through path");
    assert_eq!(n, 500);

    // 4. The incremental delta shape (`stage_incremental`'s Table arm): a
    //    DuckDB-rendered TIMESTAMPTZ watermark predicate, pushed into the
    //    scan. 1000 rows at 1h intervals from 2026-01-01; > +500h ⇒ 500.
    conn.execute_batch(
        "CREATE TEMP TABLE __delta_0 AS SELECT * FROM \
         \"__conn_pg\".\"datamk_it\".\"orders\" WHERE \"updated_at\" > \
         TIMESTAMPTZ '2026-01-21 20:00:00+00';",
    )
    .expect("incremental delta staging");
    let n: i64 = conn
        .query_row("SELECT count(*) FROM __delta_0", [], |r| r.get(0))
        .expect("delta count");
    assert_eq!(n, 500);

    // 5. The `query:` passthrough (`postgres_query`), exactly as
    //    `query_read_sql` renders it — server-side SQL, server-side rules.
    let n: i64 = conn
        .query_row(
            &format!(
                "SELECT * FROM postgres_query('__conn_pg', '{}')",
                esc("SELECT count(*) AS c FROM datamk_it.orders WHERE amount > 750")
            ),
            [],
            |r| r.get(0),
        )
        .expect("postgres_query passthrough");
    assert_eq!(n, 500);

    // 6. READ_ONLY is enforced by DuckDB on the connector's attach.
    let err = conn
        .execute_batch("INSERT INTO \"__conn_pg\".\"datamk_it\".\"orders\" VALUES (0,0,0,now());")
        .expect_err("INSERT through a READ_ONLY attach must fail");
    assert!(
        err.to_string().contains("read-only"),
        "unexpected error: {err}"
    );

    // Cleanup (best effort).
    let _ = conn.execute_batch("CALL postgres_execute('rw', 'DROP SCHEMA datamk_it CASCADE;');");
}
