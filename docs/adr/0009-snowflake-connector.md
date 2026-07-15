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

Everything below was verified live against a real Snowflake account through
the same DuckDB 1.5.4 the `duckdb` crate bundles.

## Decision

### 1. Every read is staged — `ObjectKind::Query` for everything

The extension's attach-scan reads `SELECT *` (with `WHERE` pushdown,
verified byte-correct for DATE and TIMESTAMPTZ predicates) reliably, but
cannot survive arbitrary transform SQL: a bare `SELECT COUNT(*)` over the
scan fails with pushdown enabled (the extension emits an empty projection)
*and* disabled (DuckDB: "Virtual columns require projection pushdown").
`COUNT(*)` is table-stakes transform SQL, so a read-through TEMP VIEW
binding (BigQuery's `ObjectKind::Table` path) is a loaded gun pointed at the
author's own transforms — mid-transaction, at arbitrary depth, exactly the
ADR 0006 failure mode.

So the Snowflake connector classifies **everything** as `ObjectKind::Query`:
every `table:` source — base table or view — is staged once per run into a
session-local temp table via `CREATE TEMP TABLE … AS SELECT * FROM
<attach-qualified path> [WHERE cursor > mark]`, and transforms only ever see
the staged copy. Consequences, owned openly:

- Transform-level predicate pushdown is lost (BigQuery base tables keep it).
  The run log narrates this per source, naming the two levers
  (`incremental:`, `query:`).
- N transforms referencing one source cost one Snowflake read, not N.
- The `incremental:` watermark predicate still pushes into the staging read,
  so ADR 0005's steady-state promise holds.
- `ObjectKind` is thereby restated as what it always operationally was: the
  **binding decision** (read-through vs staged), connector-defined — not a
  BigQuery storage-API taxonomy.

### 2. No classification metadata job

BigQuery's per-dataset `INFORMATION_SCHEMA` job exists to (a) route
table-vs-view, (b) work around DuckDB's info-schema lying about views, and
(c) fetch native column types for predicate rendering. None hold here:
routing is uniform (§1), the attach reads views fine, and predicates render
DuckDB-side (`MarkValue::as_literal`) with the extension owning the pushdown
translation. `classify_objects` therefore synthesizes the metadata locally —
zero warehouse round-trips — and the staging read itself is the existence
check, with its failure rewritten actionable (§5).

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
  `prepare()` so an absent secret mount crashes loud pre-attach.
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

Auth is delivered as a session-local `CREATE OR REPLACE SECRET` +
`ATTACH IF NOT EXISTS '' … (TYPE snowflake, SECRET …, READ_ONLY,
enable_pushdown true)` batch — idempotent, one attach per connection, and
per-connection secrets mean several Snowflake connections with different
keys coexist in one run (no BigQuery-style one-ADC-file-per-run constraint).
`enable_pushdown` is safe because transforms never touch the scan (§1); it
buys watermark-predicate pushdown.

### 4. Identifiers: fold to UPPERCASE, then quote

`qualify()` folds each `schema.table` part to uppercase **before**
double-quoting — Snowflake's own unquoted-identifier rule. Quoting the
author's case verbatim (BigQuery's discipline) would flip the identifiers
into case-sensitive resolution and stop `raw.events` from matching the real
`RAW.EVENTS`. Genuinely case-sensitive objects (created quoted) are
unreachable through the attach regardless (live-verified — the extension
folds server-side), so a double quote in a `table:` path is rejected with a
pointer to `query:`, the documented route to such names. Not-found errors
name the *folded* path, so the author sees what was actually looked up.

### 5. Error surface: no BigQuery vocabulary, three rewrites

Every trivial seam arm is implemented with Snowflake-appropriate text; no
"jobs API"/"~10GB ceiling"/"Storage Read API" prose can surface for a
Snowflake connection. Three failure shapes get actionable rewrites:

- **Missing ADBC driver** (at attach): what the driver is, why datamk cannot
  fetch it, the `SNOWFLAKE_ADBC_DRIVER_PATH` override, and the guide anchor
  with the per-platform install command.
- **No active warehouse** (at staging): the classic footgun when
  `warehouse:` is unset and the user has no default.
- **Table not found** (at staging): names the folded path, the role, and the
  `query:` escape for case-sensitive objects.

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
  pinned wherever baked; datamk only ever issues `SELECT *` + optional
  `WHERE`, and `snowflake_query` passthrough — the verified shapes.
- TIMESTAMP_TZ and DATE watermark round-trips are live-verified; NTZ/LTZ
  ride the same DuckDB-side comparison with the session-timezone caveat
  documented in the guide.

## Alternatives considered

- **Read-through binding for base tables** (BigQuery parity). Rejected: the
  extension's scan dies on ordinary transform shapes (§1); correctness over
  pushdown.
- **Classification via INFORMATION_SCHEMA** for honest table-vs-view labels.
  Rejected: one warehouse round-trip per schema per run for a cosmetic label
  once routing is uniform (§2).
- **Password auth.** Rejected: literal secret in a profile field, deprecated
  by Snowflake for service users, and it doesn't even serve the local-dev
  case (that want is SSO, which `externalbrowser` covers).
- **Auto-downloading the ADBC driver.** Deferred: restores "just the binary"
  but means datamk fetching a third-party native binary at runtime — needs
  checksum pinning, offline override, and a security review of its own.
- **`snowflake_query()` for `table:` sources too** (skip the attach for
  reads). Rejected: the attach is still needed for `DESCRIBE` (cursor
  validation) and gives predicate pushdown on the staging read for free;
  one mechanism per concern.
