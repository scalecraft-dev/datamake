# Sources

A cell's external inputs, bound by name as session-local views before the
transforms run.

```yaml
# cell.yaml
sources:
  raw_orders: s3://acme-lake/orders/*.parquet   # path or URI: Parquet, CSV, JSON, globs
  customers:                                    # another cell's versioned table
    cell: customers
    table: dim_customers
  crm_accounts:                                 # a warehouse table via a named connection
    connection: crm                             # → profiles/<p>.yaml connections.crm
    table: sales.accounts                       # schema.table; database comes from the connection
  spend:                                        # server-side SQL instead of a table
    connection: crm
    query: |
      SELECT advertiser_id, hour, CAST(SUM(spend) AS NUMERIC) AS spend
      FROM `${connection.project}.summarydata.spend_by_minute`
      GROUP BY 1, 2
  events:                                       # read only rows past a watermark
    connection: crm
    table: analytics.events
    incremental:
      cursor: updated_at
```

`table:` and `query:` are exactly one of. `incremental:` composes with
`table:` only. Full `incremental:` reference: [incremental.md](incremental.md).

## Connections

Connections are environment config and live in the profile.

```yaml
# profiles/prod.yaml
connections:
  crm:
    type: bigquery
    project: acme-prod-crm
    # credentials: /etc/datamk/bq-key.json   # omit for ADC
    # billing_project: acme-billing
    # staging_uri: gs://acme-scratch/datamk  # results over ~10GB
  wh:
    type: snowflake
    account: MYORG-ACCOUNT123
    user: DATAMK_SVC
    private_key_path: /etc/datamk/sf-key.p8  # or authenticator: externalbrowser (local only)
    database: ANALYTICS
    # warehouse: REPORTING_WH
    # role: REPORTING_ROLE
  pg:
    type: postgres
    host: db.internal.acme.com
    database: analytics
    user: datamk_ro
    password: ${PG_PASSWORD}                 # ${VAR} only; omit for PGPASSWORD / ~/.pgpass
    # port: "5432"
    # sslmode: require                       # default
```

Every field is `${VAR}`-expandable. Secrets must be `${VAR}` references,
never literals. Setup details: [postgres.md](postgres.md),
[snowflake.md](snowflake.md).

## How each connector reads

| | BigQuery | Postgres | Snowflake |
| --- | --- | --- | --- |
| `table:` on a base table | read-through, filter/projection pushdown | read-through, pushdown, one snapshot per build | staged in full once per run, no pushdown |
| `table:` on a view | jobs API, full materialization per run | same as base table | same as base table |
| `query:` | jobs API, free `dry_run` preflight reports bytes scanned | `postgres_query()`, no preflight | `snowflake_query()`, no preflight |
| `incremental:` predicate | baked into the jobs query | pushed into the Postgres scan | executed server-side |
| Path case | as written | case-insensitive | folded to UPPERCASE |
| Oversized results | `staging_uri:` escalates to `EXPORT DATA` | n/a, streams | n/a, streams |
| Project placeholder in `query:` | `${connection.project}` required | rejected | rejected |
| Extra install | none | none | ADBC driver |

## Gotchas

- **BigQuery `query:` must use `${connection.project}`**, not a literal project. Under split billing an unqualified name resolves against the billing project and fails with `Not found: Dataset`.
- **BigQuery `BIGNUMERIC` and wide `NUMERIC` aggregates become `VARCHAR` silently.** `CAST(... AS NUMERIC)` in the query body.
- **`query:` is not dialect-portable.** Cells on `table:` run unchanged on every warehouse.
- **A value-shaping `query:` needs its own correctness test.** `verify` runs downstream of the aggregation and cannot catch a wrong `GROUP BY`. See `tests/bigquery_query_correctness.rs`.
- **`staging_uri:` needs `storage.objects.create` and `delete`** on the prefix, plus `bigquery.jobs.create`.
- **Incremental delivery is at-least-once.** Transforms over incremental sources must be replay-safe.

Design rationale: [ADR 0003](../adr/0003-generic-source-definitions.md),
[ADR 0006](../adr/0006-non-table-warehouse-reads.md),
[ADR 0007](../adr/0007-query-connection-sources.md).
