//! Gated warehouse correctness test for ADR 0007 §5 — the compensating test
//! the ADR requires for any value-shaping `query:` source: a server-side
//! `GROUP BY`/aggregation happens upstream of `verify` and `--verify-replay`
//! both, so a wrong aggregation produces wrong-but-schema-valid numbers
//! neither check can catch. This test asserts a shaped `bigquery_query()`
//! read equals the same aggregate computed locally in DuckDB over a known
//! fixture — the only mechanism datamk has for catching that class of bug,
//! short of parsing GoogleSQL (declined, ADR 0005).
//!
//! It also covers the §4 CAST assertion: the fixture includes a `BIGNUMERIC`
//! column, `CAST(... AS NUMERIC)`'d in the aggregation exactly as the docs
//! tell authors to — proving the cast round-trips without overflow/rounding
//! for representable values, the thing the engine itself cannot check.
//!
//! Convention (ADR 0003's "gated behind credentials, skipped when absent",
//! reused verbatim by ADR 0007 §5): lives in `tests/` (Cargo's standard
//! auto-discovered integration-test directory — deliberately *not*
//! `test/integrations/`, where the rest of this crate's integration tests
//! live, so credentials-gating is visible from the path alone) and
//! early-returns, with an `eprintln!` naming the skip, unless
//! `DATAMK_TEST_BQ_PROJECT` is set. `cargo test` must stay green with no
//! credentials in the environment; a later credentialed CI pass runs it for
//! real. Fixture seeding needs BigQuery *write* access to a scratch
//! dataset — unlike every other integration test in this repo — so seeding
//! additionally gates on `DATAMK_TEST_BQ_RW_DATASET`, named as this test's
//! own unique requirement rather than folded into the first variable.
//!
//! This crate has no `[lib]` target (`datamk` is bin-only), so — like
//! `test/integrations/cli.rs` — this test cannot `use` the crate's
//! internals. It drives the `bigquery` DuckDB extension directly instead,
//! which is enough: the thing under test is "does BigQuery's own
//! aggregation match DuckDB's", not any datamk-internal code path.

use duckdb::Connection;

/// Mirrors `engine::esc` (not reachable here — no `[lib]` target): double a
/// single quote so a string survives being embedded in another single-quoted
/// SQL literal.
fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

/// Mirrors the engine's bare-project `CALL bigquery_execute(...)` renderer
/// (ADR 0006 §3a) — the one path proven live to run (and bill) a job
/// outside a `READ_ONLY` attach. Used here for fixture DDL, which needs
/// BigQuery *write* access no other integration test in this repo needs.
fn call_bigquery_execute(project: &str, stmt: &str) -> String {
    format!("CALL bigquery_execute('{}', '{}')", esc(project), esc(stmt))
}

/// A fixture row: advertiser, a plain-`NUMERIC` spend figure, and a
/// `BIGNUMERIC` cost figure — the column ADR 0007 §4 warns degrades
/// silently to `VARCHAR` past DuckDB's `DECIMAL(38,·)` range unless the
/// author `CAST`s it. Values are chosen so summing them, then rounding to
/// `NUMERIC`'s 9-decimal scale, never lands on a rounding boundary — the
/// BigQuery-side `CAST` and the DuckDB-side comparison must agree bit for
/// bit, not just "close enough".
struct FixtureRow {
    advertiser_id: &'static str,
    total_spend: &'static str, // NUMERIC literal text
    media_cost: &'static str,  // BIGNUMERIC literal text
}

const FIXTURE: &[FixtureRow] = &[
    FixtureRow {
        advertiser_id: "adv_a",
        total_spend: "10.50",
        media_cost: "5.123456789",
    },
    FixtureRow {
        advertiser_id: "adv_a",
        total_spend: "20.25",
        media_cost: "3.876543211",
    },
    FixtureRow {
        advertiser_id: "adv_b",
        total_spend: "100.00",
        media_cost: "1.000000001",
    },
];

/// The `query:` aggregation an author would write in `cell.yaml` — same
/// shape as flight-spend's, `${connection.project}` already substituted
/// (this test plays the engine's own role for that substitution, since it
/// has no resolver to call). One `SUM` per numeric column, `CAST(... AS
/// NUMERIC)` on both per §4's rule — the media_cost one is load-bearing for
/// this test.
fn aggregation_query(project: &str, dataset: &str, table: &str) -> String {
    format!(
        "SELECT advertiser_id, \
                CAST(SUM(total_spend) AS NUMERIC) AS total_spend, \
                CAST(SUM(media_cost) AS NUMERIC) AS media_cost \
         FROM `{project}.{dataset}.{table}` \
         GROUP BY 1 \
         ORDER BY 1"
    )
}

/// The identical aggregate, computed locally in DuckDB over literal values
/// mirroring `FIXTURE` exactly. This is the "known-good" side of the
/// comparison ADR 0007 §5 requires — no BigQuery involved.
fn local_aggregation_sql() -> String {
    let values = FIXTURE
        .iter()
        .map(|r| {
            format!(
                "('{}', {}::DECIMAL(38,9), {}::DECIMAL(38,9))",
                esc(r.advertiser_id),
                r.total_spend,
                r.media_cost
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    // The sums are compared as text (`::VARCHAR`): the `duckdb` crate cannot
    // fetch a DECIMAL column as a Rust `String` directly, and rendering both
    // sides through DuckDB's own decimal-to-text at the same scale keeps the
    // comparison exact rather than float-lossy.
    format!(
        "SELECT advertiser_id, \
                SUM(total_spend)::DECIMAL(38,9)::VARCHAR, \
                SUM(media_cost)::DECIMAL(38,9)::VARCHAR \
         FROM (VALUES {values}) AS t(advertiser_id, total_spend, media_cost) \
         GROUP BY 1 \
         ORDER BY 1"
    )
}

#[test]
fn query_source_shaped_aggregation_matches_local_computation() -> anyhow::Result<()> {
    let Ok(project) = std::env::var("DATAMK_TEST_BQ_PROJECT") else {
        eprintln!(
            "skipping query_source_shaped_aggregation_matches_local_computation: \
             DATAMK_TEST_BQ_PROJECT is not set (ADR 0007 §5 gated warehouse correctness test — \
             set it, plus DATAMK_TEST_BQ_RW_DATASET, to run this for real)"
        );
        return Ok(());
    };
    let Ok(dataset) = std::env::var("DATAMK_TEST_BQ_RW_DATASET") else {
        eprintln!(
            "skipping query_source_shaped_aggregation_matches_local_computation: \
             DATAMK_TEST_BQ_PROJECT is set but DATAMK_TEST_BQ_RW_DATASET is not — fixture seeding \
             needs BigQuery *write* access to a scratch dataset, this test's own unique \
             requirement (ADR 0007 §5)"
        );
        return Ok(());
    };

    let conn = Connection::open_in_memory()?;
    conn.execute_batch("INSTALL bigquery FROM community; LOAD bigquery;")?;

    let table = format!("datamk_query_correctness_fixture_{}", std::process::id());
    let qualified = format!("{project}.{dataset}.{table}");

    let rows = FIXTURE
        .iter()
        .map(|r| {
            format!(
                "STRUCT('{}' AS advertiser_id, NUMERIC '{}' AS total_spend, BIGNUMERIC '{}' AS \
                 media_cost)",
                esc(r.advertiser_id),
                r.total_spend,
                r.media_cost
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let create_fixture =
        format!("CREATE OR REPLACE TABLE `{qualified}` AS SELECT * FROM UNNEST([{rows}])");
    conn.execute_batch(&call_bigquery_execute(&project, &create_fixture))?;

    // Best-effort cleanup on every exit path (success, assertion failure via
    // `?`/panic, or a mid-test error) — a leaked fixture table is a scratch
    // artifact, not a correctness problem, but there's no reason to leave it
    // behind if we don't have to.
    let cleanup = || {
        let drop_sql = format!("DROP TABLE IF EXISTS `{qualified}`");
        if let Err(e) = conn.execute_batch(&call_bigquery_execute(&project, &drop_sql)) {
            eprintln!("warning: failed to clean up fixture table {qualified}: {e}");
        }
    };

    let result = (|| -> anyhow::Result<()> {
        let bq_query = aggregation_query(&project, &dataset, &table);
        // `::VARCHAR` on the decimal columns for the same reason as the local
        // side: exact text comparison, and the `duckdb` crate can't fetch
        // DECIMAL as `String`. BigQuery's NUMERIC lands as DECIMAL(38,9)
        // here, matching the local side's scale, so the rendered text agrees
        // when the values do.
        let bq_sql = format!(
            "SELECT advertiser_id, \
                    total_spend::DECIMAL(38,9)::VARCHAR AS total_spend, \
                    media_cost::DECIMAL(38,9)::VARCHAR AS media_cost \
             FROM bigquery_query('{}', '{}', billing_project := '{}')",
            esc(&project),
            esc(&bq_query),
            esc(&project)
        );
        let mut bq_stmt = conn.prepare(&bq_sql)?;
        let bq_rows: Vec<(String, String, String)> = bq_stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<_, _>>()?;

        let mut local_stmt = conn.prepare(&local_aggregation_sql())?;
        let local_rows: Vec<(String, String, String)> = local_stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<_, _>>()?;

        assert_eq!(
            bq_rows, local_rows,
            "shaped BigQuery read must match the locally-computed aggregate over the identical \
             fixture rows (ADR 0007 §5) — a mismatch here means either the aggregation query is \
             wrong, or the CAST(BIGNUMERIC AS NUMERIC) assertion (§4) doesn't round-trip for \
             these values"
        );
        assert_eq!(
            bq_rows.len(),
            2,
            "expected two advertiser groups, got {bq_rows:?}"
        );
        Ok(())
    })();

    cleanup();
    result
}
