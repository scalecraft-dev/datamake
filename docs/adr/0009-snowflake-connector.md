# ADR 0009 — Snowflake connector

- **Status:** Proposed
- **Date:** 2026-07-15
- **Deciders:** Datamake team
- **Author:** @scottypate

## Scope

The second warehouse connector against ADR 0003's contract: `type: snowflake`
in the profile's `connections:` map, realized via the DuckDB community
`snowflake` extension (github.com/iqea-ai/duckdb-snowflake) over the Arrow
ADBC Snowflake driver. ADR 0003 promised later connectors are "additive
connectors against this same contract; any whose realization is non-trivial
gets its own ADR" — this one is non-trivial in exactly three places: a
uniform staged read path (no storage-vs-jobs split, but also no read-through
binding at all), a two-mode auth model (key-pair and interactive SSO), and a
native driver dependency outside DuckDB's extension registry.

The realization deliberately uses **only** the extension's `snowflake_query()`
table function — SQL text delivered verbatim to Snowflake, results streamed
back over Arrow. The extension's other surface, an attached catalog whose
scan rebuilds DuckDB query fragments into Snowflake SQL, is not used at all
(§1a).

Everything below was verified live against a real Snowflake account through
the same DuckDB 1.5.4 the `duckdb` crate bundles.

## Decision

### 1. Every read is staged — `ObjectKind::Query` for everything

The extension's attach-scan cannot survive arbitrary transform SQL. The
precise failure (live-diagnosed, and present upstream at HEAD): whenever
DuckDB's optimizer needs **zero real columns** from the scan — a bare
`SELECT COUNT(*)`, a count over a pruned subquery — the scan's SQL rebuild
renders an empty projection and sends literal `SELECT  FROM <table>` to
Snowflake (error 001003, syntax error), with pushdown enabled; with it
disabled DuckDB fails client-side ("Virtual columns require projection
pushdown"). Everything else — column projection, filter pushdown, GROUP BY,
joins, CTEs, windows — works, but `COUNT(*)` is table-stakes transform SQL,
so a read-through TEMP VIEW binding (BigQuery's `ObjectKind::Table` path) is
a loaded gun pointed at the author's own transforms — mid-transaction, at
arbitrary depth, exactly the ADR 0006 failure mode.

So the Snowflake connector classifies **everything** as `ObjectKind::Query`:
every `table:` source — base table or view — is staged once per run into a
session-local temp table via `CREATE TEMP TABLE … AS SELECT * FROM
snowflake_query('SELECT * FROM <path> [WHERE cursor > mark]', <secret>)`
(§1a), and transforms only ever see the staged copy. Consequences, owned
openly:

- Transform-level predicate pushdown is lost (BigQuery base tables keep it).
  The run log narrates this per source, naming the two levers
  (`incremental:`, `query:`).
- N transforms referencing one source cost one Snowflake read, not N.
- The `incremental:` watermark predicate is baked into the staging read's
  server-side SQL, so ADR 0005's steady-state promise holds.
- `ObjectKind` is thereby restated as what it always operationally was: the
  **binding decision** (read-through vs staged), connector-defined — not a
  BigQuery storage-API taxonomy.

Should upstream fix the zero-column scan bug, read-through binding for base
tables becomes revisitable — the staging *posture* is bug-driven, not
essential.

### 1a. One read mechanism: `snowflake_query()` — no ATTACH

Every Snowflake read — plain staging, incremental delta, the cursor
`DESCRIBE`, and ADR 0007 `query:` sources — is delivered as Snowflake SQL
text through `snowflake_query(<sql>, <secret>)`. The connection is never
`ATTACH`ed. Three reasons:

- **No translation layer.** The attach-scan's DuckDB→Snowflake SQL rebuild
  is exactly where the §1 bug lives; `snowflake_query` delivers the
  connector's SQL verbatim, so that class of bug cannot exist on this path.
  datamk composes the SQL itself (validated identifiers + connector-rendered
  literals), so nothing depends on the extension's query builder.
- **The watermark predicate executes server-side byte-verbatim** (rendered
  in Snowflake dialect: `'…'::TIMESTAMP_TZ`, `'…'::DATE`, bare integers —
  live-verified) instead of relying on the scan's pushdown translation.
- **Less machinery.** No attached catalog, no `enable_pushdown` decision, no
  scan behavior to re-verify on every extension upgrade; the verified
  surface shrinks to "does `snowflake_query` deliver SQL and stream Arrow".

Table reads render an explicit three-part path
(`"DB"."SCHEMA"."TABLE"`, database from the connection, all parts
uppercase-folded per §4). Cursor validation stays on the engine's uniform
`DESCRIBE SELECT * FROM <qualified>` because `qualify()` now returns the
`snowflake_query('SELECT * FROM …', <secret>)` expression — a plain DuckDB
relation; the DESCRIBE binds via the extension's `LIMIT 0` schema probe
(live-verified: real column names and types, one cheap server round-trip).

Connection setup becomes `CREATE OR REPLACE SECRET` + a `SELECT 1` probe
through `snowflake_query` — the probe forces ADBC driver load and
authentication at the same lifecycle point ATTACH failed at, so missing
driver / bad key / SSO errors keep their attach-time rewrites, and it runs
even with no active warehouse (live-verified).

### 2. No classification metadata job

BigQuery's per-dataset `INFORMATION_SCHEMA` job exists to (a) route
table-vs-view, (b) work around DuckDB's info-schema lying about views, and
(c) fetch native column types for predicate rendering. None hold here:
routing is uniform (§1), `snowflake_query` reads tables and views through
one server-side path, and predicates render in Snowflake's own dialect
(connector-owned `snowflake_literal`, mirroring BigQuery's pattern) with no
translation in between.
`classify_objects` therefore synthesizes the metadata locally — zero
warehouse round-trips — and the staging read itself is the existence check,
with its failure rewritten actionable (§5).

### 3. Config and auth: two mechanisms, exactly one

`type: snowflake`: `account`, `database` (required — the environment root,
analog of BigQuery's `project`; table paths are two-part `schema.table`),
`user` (required by both auth mechanisms — live-verified: the extension
refuses even an externalbrowser secret without it), `warehouse` (optional —
the user's default otherwise; deploy-scale cost control is the warehouse, so
name it in deployed profiles), `role` (optional), and exactly one of:

- `private_key_path` — PKCS#8 key file, the service-account/prod shape.
  A path, never key material (the `credentials:`/`principals:` pattern);
  relative paths resolve against the cell dir; existence is checked in
  `prepare()` so an absent secret mount crashes loud before any Snowflake
  traffic.
  `private_key_passphrase` (optional) is the one field whose *value* is
  secret: it must be a `${VAR}` reference (a literal is a resolve-time
  error) and is `Debug`-redacted end to end.
- `authenticator: externalbrowser` — SSO through the user's own browser, the
  local-dev shape. Interactive by nature; **deploy pre-flight refuses it**
  (a Builder pod has no browser).

`password:` is a sentinel field: captured and rejected with guidance rather
than silently dropped by serde. No literal token ever enters the schema
(ADR 0003 §2), and password-shaped authenticators (okta, mfa) are refused on
the same grounds.

Auth is delivered as a session-local `CREATE OR REPLACE SECRET` + a
`SELECT 1` connectivity probe through `snowflake_query` (§1a) — idempotent
(the secret re-create is a session-scoped metadata write with identical
values), and per-connection secrets mean several Snowflake connections with
different keys coexist in one run (no BigQuery-style one-ADC-file-per-run
constraint). There is no ATTACH and therefore no `enable_pushdown`
decision.

### 4. Identifiers: fold to UPPERCASE, then quote

`qualify()` folds the connection's `database` and each `schema.table` part
to uppercase **before** double-quoting into the server-side SQL —
Snowflake's own unquoted-identifier rule. Quoting the author's case verbatim
(BigQuery's discipline) would flip the identifiers into case-sensitive
resolution and stop `raw.events` from matching the real `RAW.EVENTS`. A
double quote in a `table:` path is rejected with a pointer to `query:`, the
documented route to genuinely case-sensitive (created-quoted) names.
Not-found errors name the *folded* path, so the author sees what was
actually looked up.

The `incremental.cursor` column follows the same rule: folded to uppercase,
then quoted, in the watermark predicate. A created-quoted, case-sensitive
cursor column is therefore not addressable via `table:` + `incremental:` —
the same documented boundary the table path draws.

### 5. Error surface: no BigQuery vocabulary, three rewrites

Every trivial seam arm is implemented with Snowflake-appropriate text; no
"jobs API"/"~10GB ceiling"/"Storage Read API" prose can surface for a
Snowflake connection. Three failure shapes get actionable rewrites:

- **Missing ADBC driver** (at the setup probe): what the driver is, why
  datamk cannot fetch it, the `SNOWFLAKE_ADBC_DRIVER_PATH` override, and the
  guide anchor with the per-platform install command.
- **No active warehouse** (at staging): the classic footgun when
  `warehouse:` is unset and the user has no default.
- **Table not found** (at staging or the incremental DESCRIBE): Snowflake's
  own compilation error through `snowflake_query` (`002003`: `Object '…'
  does not exist or not authorized`, live-captured — the `Object '` prefix
  keeps warehouse/role typos from misdiagnosing as a missing table); the
  rewrite names the folded path, the role, and the `query:` escape for
  case-sensitive objects.

### 6. `query:` sources and the capability gaps

`query:` maps to the extension's `snowflake_query(<sql>, <secret>)` —
author-owned SQL delivered verbatim (`esc()` only), per ADR 0007. Two
deliberate gaps, stated rather than faked:

- **No dry-run preflight.** Snowflake has no free dry-run;
  `query_dry_run_sql` returns `None` and the engine skips the preflight
  silently (`bytes_scanned: None`) — an expected capability gap, never a
  warning. The user's guard is `EXPLAIN` and warehouse resource monitors.
- **No `${connection.*}` binding.** A `query:` runs with the connection's
  `database` as the session database, so unqualified names already resolve
  against it; `${connection.project}` in a Snowflake query is a resolve-time
  error saying exactly that.

### 7. No `staging_uri`, no export escalation

Results stream over Arrow ADBC; there is no analog of BigQuery's ~10GB
anonymous-result ceiling. `is_response_too_large` is constantly false, the
ADR 0006 §3a escalation is dead by construction, and the config has no
`staging_uri` field to explain.

### 8. The ADBC driver is a supply-chain artifact, not an extension

This connector breaks ADR 0003 §3's "connectors add DuckDB extensions, not
Rust dependencies" invariant in one narrow, explicit way: the extension
loads a **separate native library** (`libadbc_driver_snowflake`, from
github.com/adbc-drivers/snowflake releases) that handles the private key and
is not installable from DuckDB's registry. The invariant's substance
survives — still no Rust dependency, no cargo feature, six match arms and
two files — but the deploy story gains a clause:

- **Baked, pinned, checksum-verified** into the base image with
  `SNOWFLAKE_ADBC_DRIVER_PATH` set (Dockerfile). The fetch-at-first-run
  deferral tolerated for registry extensions explicitly does **not** extend
  to a credential-handling native binary pulled from a release page.
- Locally, the missing-driver rewrite (§5) plus the guide's one-line
  installer cover the first-hour path. datamk itself never auto-downloads
  the driver (deferred; would need its own security review).

## Consequences

- Snowflake tables, views, and server-side SQL become first-class cell
  inputs; `serve`/`verify`/snapshots untouched (sources stay session-local).
- The connector is materially *simpler* than BigQuery — no classification
  jobs, no view routing, no export escape hatch — at the cost of the
  full-read-per-run posture for non-incremental sources (§1), documented in
  the guide and narrated per run.
- Integer cursors widen: DuckDB `DECIMAL(p,0)` (Snowflake `NUMBER(38,0)`)
  now classifies as an Integer cursor engine-wide, cast to BIGINT at the
  read sites (loud past i64 range).
- Extension (v0.1.0, third-party) and driver (go/v1.11.0) versions must be
  pinned wherever baked; datamk's entire dependency on the extension is
  `snowflake_query` passthrough — the SQL inside it is datamk-composed and
  Snowflake-parsed, so extension upgrades can't change what the warehouse
  executes.
- TIMESTAMP_TZ and DATE watermark round-trips are live-verified with
  server-side literals (`'…'::TIMESTAMP_TZ`, `DATE '…'`); NTZ/LTZ ride the
  same comparison with the session-timezone caveat documented in the guide.
- The shipped community binary predates upstream's per-scan connection-pool
  fix (#49); datamk stages sources sequentially on one DuckDB connection, so
  this doesn't bite — do not add parallel Snowflake staging without
  re-verifying against a build that includes it.

## Alternatives considered

- **Read-through binding for base tables** (BigQuery parity). Rejected: the
  extension's attach-scan dies on zero-column transform shapes (§1);
  correctness over pushdown. Revisitable if upstream fixes the scan.
- **ATTACH + attach-scan for `table:` reads** (the extension's other
  surface, and this ADR's original §3 shape). Rejected after live diagnosis:
  the scan's SQL rebuild is a translation layer datamk doesn't need — its
  known bug class (§1) plus per-upgrade re-verification cost buy only a
  pushdown datamk can render itself, byte-verbatim, in the `snowflake_query`
  SQL (§1a).
- **Classification via INFORMATION_SCHEMA** for honest table-vs-view labels.
  Rejected: one warehouse round-trip per schema per run for a cosmetic label
  once routing is uniform (§2).
- **Password auth.** Rejected: literal secret in a profile field, deprecated
  by Snowflake for service users, and it doesn't even serve the local-dev
  case (that want is SSO, which `externalbrowser` covers).
- **Auto-downloading the ADBC driver.** Deferred: restores "just the binary"
  but means datamk fetching a third-party native binary at runtime — needs
  checksum pinning, offline override, and a security review of its own.
