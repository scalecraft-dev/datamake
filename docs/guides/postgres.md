# Postgres setup

`type: postgres` reads tables, views, and materialized views through
DuckDB's core `postgres` extension. No extra driver. Verified against
Postgres 16 with DuckDB 1.5.4.

`catalog: postgres://…` in a profile is datamk's own DuckLake metadata
store. A `connections:` entry with `type: postgres` is an upstream you
read from. They are unrelated.

## Connection

```yaml
# profiles/prod.yaml
connections:
  pg:
    type: postgres
    host: db.internal.acme.com
    database: analytics
    user: datamk_ro
    password: ${PG_PASSWORD}
    # port: "5432"
    # sslmode: require
```

| Field | Required | Default | Notes |
| --- | --- | --- | --- |
| `host` | yes | | hostname, not a DSN |
| `database` | yes | | `table:` paths are `schema.table` under it |
| `user` | yes | | |
| `password` | no | ambient chain | `${VAR}` only; a literal is a resolve-time error. Omit for `PGPASSWORD` / `~/.pgpass` |
| `port` | no | `5432` | |
| `sslmode` | no | `require` | all six libpq values accepted; `verify-*` use `~/.postgresql/root.crt` or `PGSSLROOTCERT` |

The default `sslmode` is `require`, not libpq's `prefer`. A local server
without TLS needs `sslmode: disable`.

## Role

Point at a replica or an analytics database. The attach is `READ_ONLY`,
but a build holds a repeatable-read transaction open for its duration.

```sql
CREATE ROLE datamk_ro LOGIN PASSWORD '…';
GRANT USAGE ON SCHEMA public TO datamk_ro;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO datamk_ro;
```

## Reads

- `table:` is read-through with filter and projection pushdown. Paths are exactly `schema.table`, no default schema, case-insensitive.
- One Postgres snapshot per build across all transforms.
- `incremental:` pushes the cursor predicate into the Postgres scan. A scale-zero `NUMERIC` cursor is rejected; use `INTEGER`/`BIGINT`.
- `query:` runs verbatim via `postgres_query()` and is staged once per run. Postgres rules apply inside the body: `search_path`, lowercase folding, double quotes for case-sensitive names. `${connection.*}` is rejected.
- No `staging_uri:`, no `query:` dry-run preflight, no interactive auth mode.

## Errors

| Error | Check |
| --- | --- |
| `password authentication failed` | `user`, `database`, `password` or ambient chain |
| `Connection refused` | `host`, `port`, VPN, pod egress |
| `database "x" does not exist` | `database` |
| `server does not support SSL, but SSL was required` | `sslmode: disable` for a non-TLS server |
| table not found | schema USAGE grant; case is never the cause |
| `permission denied for table` | the GRANT statements above |
| `relation "x" does not exist` from `query:` | `search_path` and lowercase folding apply server-side |

Design rationale: [ADR 0010](../adr/0010-postgres-connector.md).
