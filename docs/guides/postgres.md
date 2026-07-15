# Postgres sources

`type: postgres` reads tables, views, and materialized views from a Postgres
database via DuckDB's **core** `postgres` extension — the same extension
datamk already uses for metadata-DB catalogs, so there is no community
extension to fetch and no extra native driver (none of Snowflake's ADBC
ceremony). Everything below is live-verified against Postgres 16 through the
same DuckDB 1.5.4 the `duckdb` crate bundles (ADR 0010).

**One disambiguation up front:** `catalog: postgres://…` in a profile is
datamk's *own* DuckLake bookkeeping database. A `connections:` entry with
`type: postgres` is an *upstream you read tables from*. Same DBMS, two
unrelated roles — they can even be the same server, but they are configured
independently and datamk never conflates them.

## 1. The connection

```yaml
# profiles/prod.yaml (gitignored, like every non-local profile)
connections:
  pg:
    type: postgres
    host: db.internal.acme.com    # a hostname, not a URL — never a DSN
    database: analytics           # environment root — `table:` paths are schema.table under it
    user: datamk_ro               # the Postgres role datamk logs in as (required)
    password: ${PG_PASSWORD}      # pure ${VAR} only; a literal is a resolve-time error
    # port: "5432"                # default 5432
    # sslmode: require            # the default; see §2
```

```yaml
# profiles/local.yaml — same cell, a local server instead
connections:
  pg:
    type: postgres
    host: localhost
    database: analytics_dev
    user: ${USER}
    # password omitted -> libpq's ambient chain (PGPASSWORD, ~/.pgpass)
    sslmode: disable              # local server, no TLS — explicit opt-out
```

Why discrete fields and not a `postgres://` DSN: a DSN embeds the password
literally in a profile field (ADR 0003 §2 forbids exactly that), and a libpq
keyword string that fails to parse **echoes the password back in the error
text** (live-verified). The password never appears in a profile — either
it's a pure `${VAR}` reference resolved from the environment (delivered via
a session-local `CREATE SECRET`, `Debug`-redacted, and scrubbed from any
attach error), or it's omitted entirely and libpq's ambient chain applies
(`PGPASSWORD`, `~/.pgpass`) — the analog of BigQuery's ambient ADC.

**Point the connection at a read replica or an analytics Postgres, not your
production primary.** datamk's attach is `READ_ONLY` (enforced by DuckDB —
an INSERT through it fails before reaching the server) and the recommended
role has nothing but SELECT, so datamk cannot write — but reads still
contend: a build holds a repeatable-read transaction open for its duration
(§3), which on a busy primary delays vacuum. Give datamk a role of its own:

```sql
CREATE ROLE datamk_ro LOGIN PASSWORD '…';
GRANT USAGE ON SCHEMA public TO datamk_ro;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO datamk_ro;
```

## 2. `sslmode` — datamk defaults to `require`, not libpq's `prefer`

libpq's default (`prefer`) silently downgrades to plaintext when the server
offers no TLS — a dishonest default for a tool reaching remote databases, so
datamk's is **`require`**: encrypted transport unless the profile explicitly
opts out. A local/trusted server without TLS sets `sslmode: disable`. All
six libpq values are accepted (`disable`, `allow`, `prefer`, `require`,
`verify-ca`, `verify-full`); `verify-*` use libpq's ambient root-cert
locations (`~/.postgresql/root.crt`, `PGSSLROOTCERT`).

## 3. `table:` sources — read-through, with pushdown

```yaml
# cell.yaml — contract, env-free as always
sources:
  orders:
    connection: pg
    table: public.orders          # schema.table — two parts, no default schema
```

Postgres is the read-through connector: every `table:` source — base table,
view, or materialized view — binds as a TEMP VIEW over the attached
database, and transform SQL scans Postgres directly with **filter and
projection pushdown** (live-verified, including the `COUNT(*)` shape that
forces Snowflake to stage). Nothing is copied unless a transform asks for
it, and a transform that filters reads only what it filtered.

Two properties that make this safe against a live OLTP database
(live-verified under sustained concurrent writes):

- **One snapshot per build.** Within the build's transaction the extension
  pins a single Postgres snapshot across every statement — N transforms
  reading the same mutating table all see the same consistent state, and a
  single staging scan can never observe a torn, half-committed write.
- **`READ_ONLY` end to end.** DuckDB refuses any write statement through
  the attach, and the recommended role couldn't execute one anyway.

Paths are exactly two dot-separated parts; the database comes from the
connection (a cross-database read is a second connection, not a three-part
name), and there is no bare-name default to `public` — write
`public.orders`, the same one path grammar as every other connector.
`table:` paths resolve **case-insensitively** in every direction
(live-verified), so unlike Snowflake there is no fold rule to trip over;
Postgres's own lowercase fold only matters inside a `query:` body (§5).

## 4. `incremental:`

```yaml
sources:
  orders:
    connection: pg
    table: public.orders
    incremental:
      cursor: updated_at
      # lookback: 2h
```

Works exactly as ADR 0005 describes: the delta past the watermark is staged
once per run with the cursor predicate rendered DuckDB-side — and pushdown
carries it into the Postgres scan (live-verified via EXPLAIN: the
comparison executes server-side), so a delta read costs Postgres only the
delta. Types map cleanly (DuckDB's type system descends from Postgres's);
there is no BigQuery-style native-type disambiguation and no metadata job.
A scale-zero `NUMERIC` cursor is rejected as an integer cursor — Postgres
has real `INTEGER`/`BIGINT`, and a `NUMERIC` column is a genuine decimal.

## 5. `query:` sources

```yaml
sources:
  spend_by_customer:
    connection: pg
    query: |
      SELECT customer_id, sum(amount) AS spend
      FROM public.orders
      GROUP BY 1
```

The query executes **on the Postgres server verbatim** via
`postgres_query()` and the result is staged once per run (ADR 0007: no
identifier rewriting, no predicate injection, never composes with
`incremental:`). Because it runs server-side, Postgres's own rules apply
inside the body — unqualified names resolve via `search_path`, unquoted
identifiers fold to **lowercase**, and a case-sensitive (quoted-created)
object needs double quotes inside the query. There is no
`${connection.project}` analog: the query already runs against the
connection's `database`, so unqualified `schema.table` names resolve
against it (a `${connection.*}` binding in a Postgres `query:` is a
resolve-time error saying so).

## 6. Failure shapes worth knowing

All rewritten into actionable errors, live-captured (the raw cause is
always appended, never discarded):

- **`password authentication failed`** → names the user and database, and
  both password channels (`password:` and the ambient chain).
- **`Connection refused`** → names `host:`/`port:` and the
  reachability questions (VPN, pod egress).
- **`database "x" does not exist`** → names `database:`.
- **TLS mismatch** (`server does not support SSL, but SSL was required`) →
  names the connection's `sslmode`, datamk's `require` default, and the
  `sslmode: disable` local escape hatch.
- **Table not found** (through the attach) → names the database and the
  schema-USAGE requirement; case is *not* the problem (§3).
- **`permission denied for table`** → names the role and the exact GRANT
  statements to run.
- **`relation "x" does not exist`** (from a `query:` body) → explains
  `search_path` and the lowercase fold, which *do* apply server-side.

## 7. What Postgres sources don't have

- **No staged-copy-per-run tax** — the Snowflake posture doesn't apply;
  reads are read-through with pushdown (§3).
- **No oversized-result escape hatch** (`staging_uri:`) — reads stream over
  the wire protocol; there is no ~10GB jobs-result ceiling to escalate
  around.
- **No free dry-run preflight** for `query:` sources — the ADR 0007 §4
  preflight is skipped silently (an expected capability gap, not a
  failure).
- **No interactive auth mode** — nothing for deploy pre-flight to refuse;
  password/ambient auth works identically in a pod and on a laptop.
