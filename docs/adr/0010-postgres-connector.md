# ADR 0010 — Postgres connector

- **Status:** Accepted
- **Date:** 2026-07-15
- **Deciders:** Datamake team
- **Author:** @scottypate

## Scope

The third connector against ADR 0003's contract: `type: postgres` in the
profile's `connections:` map, realized via DuckDB's **core** `postgres`
extension (postgres_scanner) — the same extension the engine already loads
for metadata-DB catalogs, so unlike both predecessors there is no community
extension and no native-driver supply-chain clause (no ADR 0009 §8 analog).
Mechanically it is the cheapest connector yet: two modules, one enum
variant, one match arm per dispatch method.

It is non-trivial in exactly two places, and they are the two seams the
prior connectors never had to genuinely decide: the `ObjectKind` binding
choice (§1 — the first time it is a *choice* rather than a capability
constraint), and reconciling Postgres's native username+password auth with
ADR 0003 §2's no-literal-secret boundary (§2).

Everything below was verified live against Postgres 16 through the same
DuckDB 1.5.4 the `duckdb` crate bundles, including under sustained
concurrent write load (8 clients, sum-invariant transfer transactions
against a 2M-row table) where snapshot claims are made.

## Decision

### 1. Every read is read-through — `ObjectKind::Table` for everything

BigQuery's Table/Query split was forced by its Storage Read API taxonomy;
Snowflake's uniform staging was forced by extension fragility (ADR 0009 §1:
a bare `COUNT(*)` over its attach-scan fails). Postgres is constrained by
neither: postgres_scanner survives arbitrary transform SQL — `COUNT(*)`,
joins, window functions — over base tables, views, and materialized views
alike, with filter and projection pushdown into the scan (all
live-verified). So the choice was real, and the deciding fact is:

**Within one DuckDB transaction, the extension pins a single Postgres
snapshot across statements.** Live-verified: a ticker updating ~20×/second
read back identical values before and after a multi-second scan inside
`BEGIN…COMMIT`, and every staging scan of the mutating 2M-row table
returned the exact sum invariant. Since a build runs as one DuckDB
transaction, N transforms reading through see one consistent state of the
source — the ADR 0006 §3 atomicity hazard that motivated staging-by-default
does not exist here.

That removes the only correctness argument for staging, and staging is
operationally *hostile* to an OLTP source (a full `SELECT *` per run,
against the most time-varying source family datamk connects to), so:
`classify_objects` synthesizes `ObjectKind::Table` for everything, pure, no
metadata job. Non-incremental sources bind as read-through TEMP VIEWs
(transforms pull only the rows and columns they ask for); `incremental:`
stages only the delta, with the DuckDB-rendered watermark predicate pushed
into the Postgres scan (live-verified via EXPLAIN).

The honest cost, documented in the guide: a read-through build holds a
repeatable-read transaction on the source for its duration, which on a busy
primary delays vacuum (xmin holdback). The guidance is a read replica or
analytics Postgres — advice, not mechanism; the connector cannot detect a
primary.

### 2. Auth: discrete fields; pure-`${VAR}` password or ambient chain; never a DSN

Postgres's native auth is the literal username+password shape ADR 0003 §2
exists to keep out of profiles. The reconciliation, mirroring the two
precedents at once:

- **Discrete fields, never a `postgres://` DSN.** A DSN embeds the password
  in a profile field, and — live-verified — a libpq keyword string that
  fails to parse echoes the password back in the error text. Rejecting the
  DSN shape is a security decision, not a style one.
- **`password:` must be a single pure `${VAR}` reference** (Snowflake's
  `private_key_passphrase` rule verbatim: literal → resolve-time error,
  embedded-var → error, empty expansion → loud error, value `Redacted`).
  Delivered via a session-local `CREATE SECRET` with `''`-escaping
  (live-verified intact with `p'a''ss$ w{}rd%`), referenced by the ATTACH,
  and scrubbed (plain and SQL-escaped) from any attach error text.
- **Omitted `password:` ⇒ libpq's ambient chain** (`PGPASSWORD`,
  `~/.pgpass`) — the analog of BigQuery's ambient ADC. Credentials are
  per-connection secret material (Snowflake's plane, not BigQuery's
  process-global one): `credentials()` is `None`, and connections with
  different passwords coexist in one run.
- **`user:` is required** — libpq's OS-username fallback is never right in
  a pod; the resolve error names the read-only-role shape.
- **`sslmode` defaults to `require`, not libpq's `prefer`.** `prefer`
  silently downgrades to plaintext — a credential-over-the-wire leak
  dressed as a convenience default. The value is resolve-time-validated
  against libpq's closed set and carried in the ATTACH path string (the
  secret carries everything else). Local no-TLS servers opt out explicitly
  with `sslmode: disable`; the mismatch error names both directions.

Deferred, on the same grounds ADR 0007 defers features until a real need:
`password_file:` (k8s injects env from secrets, so `${VAR}` covers deploy),
IAM auth (RDS/Cloud SQL token flows), and mTLS client-cert fields.

### 3. Table paths: `schema.table`, quoted verbatim — no fold, no `public` default

Two non-empty dot-separated parts, like both predecessors; the database
comes from the connection. No one-part default to `public` — one path
grammar across connectors beats saving six characters, and the error
teaches the fix (`public.orders`). Parts are quoted **verbatim with no case
fold**: DuckDB resolves identifiers against the attached Postgres catalog
case-insensitively in every direction (live-verified: `SALES.ORDERS`,
unquoted `CamelTable`, and quoted lowercase all resolve), so Snowflake's
fold machinery has no analog here. A double quote in a path is rejected
(resolution is case-insensitive, so quoting can only be operator error).
Postgres's own lowercase fold applies only inside `query:` bodies, which
run server-side verbatim — the `relation does not exist` rewrite for
`query:` sources explains it there, and only there.

### 4. `query:` sources: `postgres_query()`, staged, no dry-run, no `${connection.project}`

ADR 0007 §2 verbatim: the author's SQL is `esc()`'d into
`postgres_query('<attach-alias>', '<sql>')` (signature live-verified),
executes server-side, and stages once. No free dry-run exists (`EXPLAIN`
estimates rows but validates nothing a real read wouldn't), so the §4
preflight is skipped silently. `${connection.project}` is a resolve-time
error — and adding the third connector exposed that this error's text
hardcoded "snowflake"; it now names the source's actual connector type
(`substitute_connection_bindings` takes `type_name()`).

### 5. No oversized-result machinery

Reads stream over the wire protocol; there is no jobs-API result ceiling.
`is_response_too_large` is constantly false, `staging_uri()` is `None`, and
both `export_sql` shapes are unreachable bails — Snowflake §7 verbatim.

### 6. Testing: the first connector CI can exercise for real

BigQuery/Snowflake correctness tests gate on live cloud credentials and are
skipped in ordinary CI. Postgres needs only a disposable server, so
`tests/postgres_connector.rs` (gated on `DATAMK_TEST_PG_HOST`, one
`docker run` away) executes every SQL shape the connector renders — the
secret+ATTACH batch (idempotence included), read-through binding under the
transform shapes that disqualified Snowflake, the incremental delta
predicate, the `postgres_query()` passthrough, and `READ_ONLY`
enforcement — against a real Postgres through the bundled DuckDB. Wiring it
into CI as a service container is deliberately left to a follow-up; the
test itself is the contract.

## Consequences

- Postgres sources get the best read semantics of the three connectors
  (read-through + pushdown + pinned snapshot) at the lowest deploy cost
  (core extension, no driver, no supply-chain clause).
- The read-replica guidance is load-bearing for busy primaries and lives in
  the guide, the init scaffold comment, and this ADR — datamk cannot
  enforce it.
- The staged-read narrations (`stage_narration` and siblings) are
  unreachable under this classification but answer coherently — a future
  routing change fails soft, not via panic.
- If a future need arises for an opt-in staged mode (e.g. releasing the
  source transaction early on a fragile primary), it composes as a
  connection-level flag flipping `classify_objects` to `ObjectKind::Query`
  — the engine already routes both kinds.
