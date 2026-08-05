# Sources

A cell's external inputs, bound by name as session-local views before
transforms run. Three kinds:

```yaml
sources:
  raw_orders: s3://acme-lake/orders/*.parquet   # a raw path/URI (Parquet/CSV/JSON, globs ok)
  customers:                                    # another cell's versioned table
    cell: customers
    table: dim_customers
  crm_accounts:                                 # a warehouse table via a named connection
    connection: crm                             # -> the profile's `connections.crm`
    table: sales.accounts
```

*Which table* is contract (`cell.yaml`); *which project and credentials* is
environment. The profile supplies the connection, so the same cell reads a
sandbox project in dev and the real one in prod:

```yaml
# profiles/prod.yaml
connections:
  crm:
    type: bigquery                # connectors today: bigquery, postgres, snowflake
    project: acme-prod-crm
    # credentials: /etc/datamk/bq-key.json   # service-account key; omit to use ADC
    # staging_uri: gs://acme-bq-staging/datamk-scratch  # oversized view reads only; see below
  wh:
    type: snowflake               # see docs/guides/snowflake.md (needs the ADBC driver)
    account: MYORG-ACCOUNT123
    user: DATAMK_SVC
    private_key_path: /etc/datamk/sf-key.p8  # key-pair auth; local dev can use
    database: ANALYTICS                      # `authenticator: externalbrowser` instead
    # warehouse: REPORTING_WH
  pg:
    type: postgres                # see docs/guides/postgres.md (core extension, no driver)
    host: db.internal.acme.com
    database: analytics
    user: datamk_ro               # a read-only role — point at a replica, not your primary
    password: ${PG_PASSWORD}      # pure ${VAR} only; omit for PGPASSWORD/~/.pgpass
    # sslmode: require            # the default (encrypted transport, unlike libpq's `prefer`)
```

## How each connector reads

On BigQuery and Postgres, transforms filter through the view with full
pushdown — write plain SQL against `crm_accounts` and DuckDB pushes
projections/filters into the warehouse scanner. Postgres reads everything —
tables, views, materialized views — through that one read-through path, with
the whole build pinned to a single consistent snapshot (see
docs/guides/postgres.md). (Snowflake sources are instead staged once per
run — tables and views alike, no transform pushdown; `incremental:` and
`query:` bound the read. See docs/guides/snowflake.md.)
View-backed connection sources (a BigQuery view, materialized view, or
external table) are auto-detected and read via the BigQuery jobs API instead
— no DuckDB pushdown; the full view materializes every run unless
`incremental:` bakes the watermark predicate into the issued query. If a
jobs-API result exceeds BigQuery's ~10GB anonymous-result ceiling, set
`staging_uri:` (a scratch object-store prefix) to escalate to `EXPORT DATA`
instead of failing — needs `storage.objects.create`/`delete` on that prefix
for the warehouse identity, in addition to `bigquery.jobs.create`.

## `query:` sources — server-side shaping

A `connection` source may declare **`query:`** — warehouse-dialect SQL
executed server-side — instead of `table:`. It's the escape hatch for a
source scan-with-pushdown can't express, most often a value-shaping
aggregation that shrinks what crosses the wire (and what's billed to be
*returned*, not scanned):

```yaml
sources:
  raw_spend_hourly:
    connection: dw_silver
    query: |
      SELECT advertiser_id, parent_campaign_group_id, campaign_group_id,
             hour, CAST(SUM(total_spend) AS NUMERIC) AS total_spend,
             CAST(SUM(media_cost) AS NUMERIC) AS media_cost
      FROM `${connection.project}.summarydata.campaign_group_spend_by_minute`
      GROUP BY 1, 2, 3, 4
```

`query:` and `table:` are exactly-one-of. `query:` is jobs-routed by
construction — same staging, same ceiling escalation, same narration as
a view — and never composes with `incremental:` (v1; a `query:` source
re-reads in full every run). Before the real read, a free `dry_run := true`
preflight fails loud on a clearly-malformed query and narrates the exact
`total_bytes_processed` the real read will scan; after staging, the engine
DESCRIBEs the temp table and logs its column types. Four things worth
stating plainly:

- **Never hardcode a project in the query — write `${connection.project}`.**
  This is a reserved, engine-owned binding (env-var names can't contain
  `.`, so there's no collision with `${VAR}` expansion): the engine
  substitutes the connection's resolved `project` at resolve time,
  fully-qualifying `` `${connection.project}.dataset.table` `` the same way
  `qualify()` fully-qualifies a `table:` reference. This isn't just style —
  an unqualified `dataset.table` in a BigQuery job resolves against
  whichever project the job *runs in*, which under split billing
  (`billing_project` set and different from `project`) is the *billing*
  project, not the storage project, and fails loud with `Not found:
  Dataset <billing>:<dataset>`. Any other `${connection.*}` name is a
  resolve-time error naming the one supported binding.
- **A `query:` source narrows portability** to that warehouse family —
  GoogleSQL in `cell.yaml` isn't dialect-portable the way `table:` is.
  Cells that stay on `table:` keep the full same-cell-everywhere guarantee.
- **BigQuery numerics beyond DuckDB's range degrade silently to
  `VARCHAR`.** `BIGNUMERIC` (and full-range `NUMERIC` aggregates, e.g.
  `SUM(BIGNUMERIC)`) exceed DuckDB's `DECIMAL(38,·)`; the extension maps
  such columns to `VARCHAR` with no warning, and the engine cannot detect
  this authoritatively (there's no extension-exposed result schema for an
  arbitrary query). `CAST(… AS NUMERIC)` in the query body is the fix — and
  an **author correctness assertion** (it can overflow or round), not an
  engine check, so it's covered by the gated test below, not by anything
  automatic.
- **A value-shaping `query:` source needs a gated warehouse correctness
  test.** `verify` and `--verify-replay` both run downstream of the
  aggregation — a wrong `GROUP BY` produces wrong-but-schema-valid numbers
  neither check can catch. Assert the shaped read against a known fixture
  (see `tests/bigquery_query_correctness.rs`, gated on
  `DATAMK_TEST_BQ_PROJECT`).

## Incremental loading

A `connection` source can declare a cursor so the Builder reads only rows
past a persisted watermark instead of re-scanning the whole table every run.
Incremental works over views too (the predicate is baked into the jobs-API
query) and is the only way to avoid a full read per run for a view — though
bytes *billed* still depend on the view's own SQL (a CDC-dedup view may scan
its full base table regardless of the predicate), even though bytes
*returned* scale with the delta by construction:

```yaml
sources:
  events:
    connection: crm
    table: analytics.events
    incremental:
      cursor: updated_at   # a monotonic column; its max is the new watermark
```

Delivery is **at-least-once** — a transform reading an incremental source
must be replay-safe (an anti-join or `MERGE`, never `CREATE OR REPLACE`). See
[Incremental source loading](incremental.md) for the full guide,
`--full-refresh`/`--verify-replay`, and the edge cases.
