# ADR 0006 — Reading non-table warehouse entities: jobs-API routing for views

- **Status:** Proposed
- **Date:** 2026-07-13
- **Deciders:** Datamake team
- **Author:** @scottypate
- **Depends on:** ADR 0003 (connection sources), ADR 0005 (incremental
  source loading)

## Scope

This ADR adds a **second read mechanism** for `connection` sources: objects
the warehouse's bulk-read API cannot serve — BigQuery views, materialized
views, external tables — are detected at bind time and read through the
warehouse's **jobs API** (`bigquery_query()`), with any incremental
watermark predicate rendered into the issued warehouse-dialect SQL. It
covers detection, routing, staging, the dialect seam, the permission and
cost model, and the revisions this forces on stated invariants of ADRs
0003 and 0005. Nothing in `cell.yaml` or the profile changes shape.

## Context

### The failure

A production cell sourcing four BigQuery objects failed on its first
transform: every source is a **view** (a sqlmesh virtual-layer surface and
silver-layer CDC views), and the engine's one read path for connection
sources — `SELECT * FROM <qualified>` against the attached scanner
(`engine::bind_source`) — is a **BigQuery Storage Read API** scan. That
API refuses non-table entities outright:

```
Binder Error: Error while creating read session: Permanent error, with a
last message of request failed: non-table entities cannot be read with the
storage API
```

The bind *succeeded* — `ATTACH` and `DESCRIBE` are REST metadata — so the
failure landed mid-transaction, on the first transform that scanned the
source. ADR 0003 §3 hardcoded an unstated assumption into the connector
seam: *every object is storage-scannable and the read is always
`SELECT * FROM <qualified>`*. That assumption was never part of the
contract; it was a default that happened to hold for base tables. This ADR
makes "how do I read this object" an explicit connector answer.

Repointing sources at the physical tables behind the views is not an
option: sqlmesh physical names carry version fingerprints that change on
every plan, and the silver views do CDC dedup the raw tables don't.

### Facts established empirically (live, against the real warehouse)

These probes were run against the affected views with the community
`bigquery` extension before this design was fixed; each one eliminated a
design option or confirmed a mechanism:

1. **The attached catalog lies about views.** DuckDB-side
   `information_schema.tables` reports a real BigQuery view as
   `BASE TABLE`; `duckdb_views()` is empty. Local metadata detection is
   impossible — detection must ask BigQuery itself.
2. `DESCRIBE SELECT * FROM <alias>.<ds>.<view>` resolves via REST with no
   storage read and no job — cursor existence/type validation keeps
   working for views unchanged.
3. `bigquery_query('<project>', '<GoogleSQL>', billing_project := …)`
   works: `INFORMATION_SCHEMA` lookups, and reads with fully-qualified
   backticked tables and baked typed predicates, all verified.
4. **BigQuery `TIMESTAMP` and `DATETIME` both surface as DuckDB
   `timestamp`** (naive, UTC values). DuckDB metadata cannot choose the
   GoogleSQL literal keyword — and comparing a `DATETIME` column to a
   `TIMESTAMP` literal is a hard type error in BigQuery. Literal rendering
   must key on the BigQuery-native `data_type`.

## Decision

### 1. Detection: authoritative, per-run, no config field

At bind time — pre-`BEGIN`, before any read path is chosen — the engine
classifies every connection source's object through **one warehouse
metadata job per `(connection, dataset)` per run**, cached for the run:
an `INFORMATION_SCHEMA.TABLES` (+ `COLUMNS`, §4) query issued through
`bigquery_query`. BigQuery's `table_type` is ground truth; it cannot
drift and cannot be wrong.

**No `view:`/`read_via:` field exists, in `cell.yaml` or the profile.**
The same `dataset.table` reference is legitimately a physical table in a
dev sandbox and a view in prod (the sqlmesh case) — so in `cell.yaml` a
view flag is a contract defect that breaks the same-cell-everywhere seam,
and in the profile it is an environment declaration that silently drifts
into a lie the day the object flips. Detected-fresh-every-run dominates
declared-and-can-drift. The author's legitimate need to *see* the cost
model is served by narration (§6), not configuration. If detection ever
proves inadequate, the reserved break-glass shape is a profile-connection
`read_via: auto | jobs | storage` — environment, coarse, documented as a
workaround; not shipped now.

**No try-storage-then-fall-back heuristic.** The failure asymmetry
forbids it: wrong-toward-table is today's loud mid-transaction death, but
wrong-toward-jobs is *silent* — a huge base table full-materialized
through a billable job with the storage path's pushdown quietly gone,
which for an incremental source is exactly the "worse than today"
regression ADR 0005's Consequences forbid. Routing is decided by the
warehouse's own classification, never by catching errors.

### 2. The routing table is a closed set

| `table_type`                        | Path                          |
|-------------------------------------|-------------------------------|
| `BASE TABLE`, `CLONE`, `SNAPSHOT`   | storage scan (today's path, byte-identical SQL) |
| `VIEW`, `MATERIALIZED VIEW`, `EXTERNAL` | jobs API (`bigquery_query`) |
| anything else                       | **hard error at bind**, naming the value |

An unknown type is not warned-and-routed: over-routing an unrecognized
object to the jobs API is the same silent-degradation trap §1 rejects,
and because classification is pre-`BEGIN`, failing loud dies at bind —
earlier and cleaner than either alternative. Same closed-enum posture as
`type:` and `--target`.

### 3. Jobs-path reads stage exactly once

A jobs-routed source — plain or incremental — stages into a local
`TEMP TABLE` (`CREATE TEMP TABLE __jobs_<idx> AS SELECT * FROM
bigquery_query(…)`) with the source's `TEMP VIEW` bound over it. A
`TEMP VIEW` directly over `bigquery_query(…)` would re-run a full
BigQuery job on every transform that scans the source — N transforms →
N bills — and, worse, could hand *different rows to different transforms
in the same build* from a time-varying view: a soft violation of build
atomicity. This is ADR 0005 §Alternatives' rejected "filter in the view,
no local staging" shape, rejected here for the same reason.

ADR 0003 §3's "views are inlined so pushdown works" clause is **void on
the jobs path regardless** — the query string is opaque to DuckDB; there
is no scanner to push into — so staging costs nothing that wasn't
already gone. Every other property of §3 (declared inputs, the engine-
internal attach alias, lineage without parsing SQL) is preserved
untouched. Base-table sources keep today's pass-through `TEMP VIEW` and
its proven pushdown, byte-identical.

Staging a plain view is a full local materialization; the spill
configuration ADR 0005 scoped for incremental bootstrap
(`temp_directory` + memory limit) is a **precondition for plain view
sources too**.

### 3a. Oversized jobs-path results: EXPORT DATA to engine-owned storage

§3's staging silently assumed the job's result fits BigQuery's anonymous
result table. It doesn't always: that table has a ~10GB ceiling, and a
live run against a real 2.57B-row view failed on it (`Response too large
to return. Consider specifying a destination table in your job
configuration` — surfaced by the extension, misleadingly, as a permission
error). The ceiling is structural; patience does not help.

**Unlike object kind, result size is not classifiable up front.** A view
has no row count; there is no pre-`BEGIN` probe that reveals "this result
exceeds 10GB" short of running the query. §1's
deterministic-routing-never-catch posture therefore cannot extend to the
ceiling case, and the only honest trigger is the loud structural error
itself. Catch-and-escalate is defensible here where §1's
catch-and-fallback was not, because the two branches read the **same
view with the same baked predicate** — the only difference is where the
byte-identical result set materializes (BigQuery's anonymous table vs. a
scratch prefix in object storage). There is no cheaper-but-wrong outcome
to mask; a wrong escalation costs an unneeded export, loudly, never
silently.

The mechanism, on the exact ceiling-error signature only (anything else
re-raises unchanged):

1. Run `EXPORT DATA OPTIONS (uri := '<staging_uri>/<cell>/<run>/*.parquet',
   format := 'PARQUET') AS <the same GoogleSQL SELECT §4's read_sql
   builds>` as a jobs-API statement — same backtick qualification, same
   predicate rendering, reused verbatim.
2. Stage with `CREATE TEMP TABLE __jobs_<idx> AS SELECT * FROM
   read_parquet('<staging_uri>/<cell>/<run>/*.parquet')` through the
   `gs://` read path the engine already owns.
3. Delete the scratch prefix.

All of it pre-`BEGIN`, like the rest of staging: a failed export or read
fails the bind loudly, and the snapshot's one-commit invariant is
untouched. The connector grows one more **pure** renderer (`export_sql`,
sibling to `read_sql`); the engine owns the sequencing and side effects,
preserving §4's purity split.

**Why not a destination table in the warehouse** (BigQuery's own
suggestion): that is a warehouse *write* — `tables.create`/`tables.delete`
on a scratch dataset, a non-read-only handle, and a cleanup story leaning
on a foreign dataset's default expiration datamake cannot own. ADR 0003's
"datamake never writes to a warehouse" holds by the letter. EXPORT DATA
writes only to object storage — the thing datamake already writes its
entire lakehouse to on every run. Of ADR 0003's three objections to
extract-to-parquet as a *core* mechanism, two evaporate for the jobs
path: the staging location is datamake's own prefix (not one it "knows
nothing about"), and there is no scanner pushdown to re-implement
(§3 — it never existed on this path). The third (transient double
storage) is accepted for the oversized minority only.

**Config:** one new optional **profile** field on the BigQuery
connection — `staging_uri:` (e.g. `gs://acme-bq-staging/datamk-scratch`).
Environment, connection-scoped, never `cell.yaml` (same reasoning that
keeps `project` out of the contract). Not derived from `storage:`:
`storage:` may be local (BigQuery cannot export to a laptop), the GCS
write grant belongs to the *warehouse's* service identity (a different
credential plane from datamake's store client), and export cost/latency
wants the staging bucket co-located with the dataset's region
independently of where the cell's storage lives. A view that hits the
ceiling with no `staging_uri` configured is a hard error naming the
field and the fix.

**Permissions:** the warehouse identity needs `storage.objects.create`
on `staging_uri` (in addition to §5's `bigquery.jobs.create`); datamake's
reader needs read, and cleanup needs `storage.objects.delete`. Rides on —
does not solve — the one-ADC-identity-per-run caveat.

**Watched failure modes:** a crash between export and delete orphans
scratch parquet (run-scoped paths make orphans identifiable; an
object-lifecycle rule on the prefix is the datamake-ownable
belt-and-braces); the escalation trigger is a string match on BigQuery's
message and a reword would stop escalation (re-raised unchanged — loud,
not silent); parquet round-trip is a **third** type-delivery path
(NUMERIC/BIGNUMERIC especially) and the gated warehouse test must assert
it classifies identically to the direct jobs read; and the staged result
still fully materializes locally — the §3 spill precondition is
non-optional here, and a >10GB staged source is the standing argument
for migrating that source to server-side shaping (ADR 0007) once it
exists.

**What this section is not:** a substitute for `incremental:` — and
`incremental:` is not a substitute for it. An incremental source reads
*unbounded* on bootstrap and on every `--full-refresh` (ADR 0005 §1), so
a large source's bootstrap needs this mechanism regardless of its
steady-state cursor. And a source whose consumer only wants an aggregate
of it (the live case: minute-grain spend consumed at hour grain) is
better served by ADR 0003's deferred `query:` source — reopened as ADR
0007 on the strength of this exact evidence, per 0003's own reopening
condition ("if scan-with-pushdown proves insufficient for a real
table"). This section is the floor that works with zero contract change;
0007 is the destination that makes the floor rarely needed.

### 4. The connector seam grows two answers, split by purity

The engine must not learn what a "Storage Read API" is. It asks the
connector; the connector routes internally. Per ADR 0003's "a connector
is N answers" seam (preserving its string-equality testability):

- **impure** — `classify_objects(…) -> Map<Table, ObjectMeta>`: the one
  live metadata probe (per-dataset, batched, run-cached). `ObjectMeta`
  carries the object kind **and the columns' warehouse-native types**.
- **pure** — `read_sql(alias, table, meta, predicate: Option<CursorPredicate>)
  -> String`: returns the staging `SELECT`. The table branch reproduces
  today's DuckDB-dialect SQL verbatim; the jobs branch emits
  `bigquery_query('<project>', '<GoogleSQL>', billing_project := …)`
  with backtick-qualified `` `project.dataset.table` `` and the predicate
  rendered in GoogleSQL.

The GoogleSQL identifier is a *second* qualification discipline —
backticks, project pinned by the function's first argument — genuinely
distinct from `qualify()`'s DuckDB three-part double-quoted form.
Reusing `qualify()` in the query string would be a bug; the asymmetry is
the proof this is a real new connector concern, not a special case.

**The dialect seam is the connector, not `MarkValue`.** The engine's
watermark bookkeeping stays dialect-free: `read_watermark` already folds
lookback into a single adjusted lower bound, so the connector receives
one final `(cursor, MarkValue)` and renders `cursor > <literal>` in its
own dialect. Because DuckDB metadata cannot distinguish `TIMESTAMP` from
`DATETIME` (fact 4), the literal keyword keys on the BigQuery-native
`data_type` fetched by `classify_objects`:

| BQ `data_type` | GoogleSQL literal                          |
|----------------|--------------------------------------------|
| `TIMESTAMP`    | `TIMESTAMP '<value>+00'`                   |
| `DATETIME`     | `DATETIME '<value>'` (offset stripped)     |
| `DATE`         | `DATE '<value>'`                           |
| `INT64`        | bare integer                               |
| anything else  | hard error at bind, naming the type        |

Cursor existence/type validation stays on the attached-catalog
`DESCRIBE` (fact 2 — free, no job). The watermark *value* remains
self-consistent by construction: `max(cursor)` is computed from the
staged jobs-API data, so it round-trips through the same delivery path
it will be compared against next run.

### 5. Classification denied: degrade, don't gate — but fail at bind

The probe needs `bigquery.jobs.create` on the billing project — the same
permission the jobs path itself needs, so it is coherent with the fix,
not an added requirement. A caller without it (a pure storage-API reader
over base tables) must keep working with zero new requirements:

- Probe denied → **warn once per connection** and assume `BASE TABLE`
  (today's path).
- On that denied path only, a pre-`BEGIN` `SELECT … LIMIT 1` probe
  through the attach moves a real view's failure to **bind time**, where
  it is rewritten to name the cause and the fix (grant `jobs.create`, or
  point `table:` at a base table) — never the raw extension error, and
  never mid-transaction.

### 6. Narration

Routing is a run-time fact and is narrated per source at bind, in ADR
0005 §4's voice — `info` for both view arms (a full read of a small view
is a valid steady state; a recurring `warn` on a legitimate configuration
trains operators to ignore warnings — a deliberate divergence from the
no-grain warning, which flags a correctness gap, not a cost choice):

```
source 'flights' is a view (ui.ui_flights) — reading via the BigQuery jobs
API (full view materialized every run). Add `incremental:` with a cursor to
read only new rows.
```

`datamk status` is unchanged: it inspects catalog state offline, has no
live warehouse handle, and must not grow one to report a `run`-time fact.
(ADR 0007's dry-run preflight later makes this narration *exact* for
`query:` sources — per-run bytes-scanned reported at bind.)

### 7. What does not change

- `cell.yaml` and profile schemas: **zero new fields**.
- Base-table sources: byte-identical SQL, storage pushdown, ADR 0005's
  bytes-scanned test — all untouched.
- `serve`, the interface contract, snapshot pinning, `verify` (offline),
  and the watermark bookkeeping (`__datamk_watermarks`, `MarkValue`,
  `persist_watermarks`).
- `--verify-replay` stays cheap for views *because of* §3's staging: the
  replay re-runs transforms against the already-staged local copy; the
  warehouse is never re-billed.

## Consequences

- **View-backed sources work.** The blocked production pattern — sqlmesh
  virtual layers and silver CDC views as cell inputs — binds and runs.
- **A new cost class, stated honestly.** Predicate-in-query-string
  guarantees bytes *returned* scale with the delta by construction; it
  does **not** guarantee bytes *scanned/billed* — that depends on the
  view's own SQL, which datamk cannot see. A CDC-dedup view (window
  function over the base table) may scan its full base table every run
  while returning only the delta. ADR 0005's bytes-scanned shipping
  precondition is therefore **not satisfied by construction for views**;
  the gated warehouse test can only assert it per known view, and the
  docs state the degradation per this ADR rather than marketing
  incremental-over-views as proven scan-cost reduction.
- **"Read-only" now includes "runs billable jobs."** Jobs-path reads bill
  the connection's `billing_project` (defaulting to `project`), which is
  now load-bearing: it must hold `bigquery.jobs.create`. Bootstrap and
  every `--full-refresh` of a view source are full-view jobs.
- **Detection costs one metadata job per (connection, dataset) per run**,
  on every cell with connection sources — including all-base-table cells
  that never route to jobs. Accepted over tracking readability state
  across runs; noted so `INFORMATION_SCHEMA.JOBS` readers aren't
  surprised.
- **Oversized results add a permission class and a disk precondition.**
  §3a's escape hatch needs `storage.objects.create/delete` on a scratch
  prefix for the warehouse identity, and the Builder needs local disk to
  hold and stage a >10GB export — the spill configuration is a hard
  precondition there, and hourly-staging a source that size is the
  signal to move it to ADR 0007's server-side shaping.
- **Type fidelity across read paths is a watched risk.** The storage path
  delivers Arrow; the jobs path delivers a job result set. A source that
  is a table in dev and a view in prod may surface subtly different
  staged column types (NUMERIC scale, timestamp flavors) to the same
  transform SQL. The gated warehouse test must assert both paths classify
  the same logical columns identically, and ADR 0005's no-gap timestamp
  round-trip must be re-proven **on the jobs path specifically**, with a
  `DATETIME` cursor as well as a `TIMESTAMP` one — the GoogleSQL
  reformat is a strictly higher-risk boundary than the within-DuckDB
  round-trip its existing test covers.
- ADR 0003 §3's pushdown-via-inlining sentence and §5's single-read-path
  description are amended with pointers here; ADR 0005's Consequences
  gain the scan-cost caveat above.
- One run still supports one ADC identity (`connectors::prepare`
  unchanged); jobs-path and storage-path permissions differing per
  service account is documented, not solved.

## Alternatives considered

- **A `view:`/`read_via:` field in `cell.yaml`.** Rejected: environment
  fact in the env-free contract; false in one environment the day it
  ships (sqlmesh dev-table/prod-view); breaks ADR 0003's portability
  seam.
- **A `read_via:` field in the profile.** Deferred as break-glass only:
  declared-and-can-drift loses to detected-fresh-every-run, and the
  drift failure is this ADR's original bug.
- **Local-catalog detection.** Disproven empirically (fact 1): the
  extension reports views as `BASE TABLE`. Any design built on it would
  reproduce the bug while appearing to handle it.
- **Try-storage-catch-fallback.** Rejected: masks permission/existence
  errors as "must be a view"; a transient storage hiccup silently routes
  a huge table through a billable full-materialization (§1 asymmetry).
- **`TEMP VIEW` directly over `bigquery_query(…)` (no staging).**
  Rejected: N transforms → N full-view jobs → N bills, plus
  non-deterministic inputs within one build (§3).
- **A `filter:` field on connection sources.** Rejected: it is ADR 0003's
  deferred `query:` source in a trench coat — author-authored
  warehouse-dialect SQL in the contract file, non-portable across
  environments, unvalidatable offline. The sanctioned predicate is
  `incremental:` — a data property, contract-safe, connector-rendered.
  Genuine dialect-SQL needs reopen `query:` as its own decision.
- **Unknown `table_type` → route to jobs with a warning.** Rejected:
  same silent-degradation trap as the fallback heuristic; pre-`BEGIN`
  classification makes fail-loud-at-bind strictly better (§2).
- **Destination-table two-hop for oversized results**
  (`bigquery_execute(…, destination_table := scratch.tbl)`, storage-scan
  it back, drop it). Rejected: a warehouse write by the letter —
  `tables.create`/`tables.delete`, a non-read-only handle, and cleanup
  hostage to a foreign dataset's expiration policy. §3a's EXPORT DATA
  reaches the identical result with every write landing in storage
  datamake already owns and cleans.
- **Cursor-range chunking for oversized results** (N sequential jobs
  under the ceiling). Rejected: for the CDC-dedup view shape that
  already rescans its full base per read, chunking multiplies that full
  scan by N — the worst possible bill — and needs a chunkable column
  plus range statistics datamake would have to discover.
- **Unconditional EXPORT for all view reads** (deterministic, no error
  match). Rejected: imposes the GCS-write permission, double storage,
  and cleanup on the small views the plain jobs path serves fine, and
  makes `staging_uri` a requirement for any view read at all.
- **Repointing sources at physical tables behind the views.** Rejected:
  sqlmesh physical names are version-fingerprinted and change every
  plan; silver views carry CDC dedup the raw tables lack.
- **GoogleSQL rendering as a `MarkValue` method.** Rejected: scatters
  warehouse dialect into the engine's generic watermark bookkeeping; the
  connector owns every other dialect concern and owns this one (§4).
