# Incremental source loading

`connection` sources can declare an `incremental:` block so the Builder reads
only rows past a persisted high-water mark instead of re-scanning the whole
upstream table every run. The design decisions and their trade-offs live in
[ADR 0005](adr/0005-incremental-source-loading.md); this page is the
operating guide — what to write, what the engine promises, and where the
sharp edges are.

## 1. When to use it

Incremental loading is for **append-heavy warehouse sources**: an events
table, an ingestion log, anything where rows are added (and rarely mutated)
behind a monotonic column. It is not a general-purpose optimization and it is
not CDC — deletes and hard updates behind the cursor are invisible to it (see
[Edge cases](#8-edge-cases-stated-plainly)).

Full-rebuild stays the right default for small upstreams and for transforms
that are cheaper to re-run in full than to make replay-safe. Reach for
`incremental:` when the table is large enough that re-scanning it every run
is the dominant cost of the cell — not by default.

## 2. The `incremental:` block

```yaml
sources:
  events:                       # cell.yaml — contract, env-free as always
    connection: crm
    table: analytics.events
    incremental:
      cursor: updated_at        # a monotonic column in the source table
      lookback: 2h              # optional; time cursors only
```

- **`cursor`** names a column whose values only grow for new/changed rows
  (a timestamp, an autoincrement id, an ingestion timestamp). It's a
  property of the *data*, so it lives in `cell.yaml` (contract), never in a
  profile. Supported cursor types are a closed set — **timestamp, date, or
  integer** — checked against the source's live column type when the source
  is bound (see [Verifying replay-safety](#6-verifying-replay-safety) for what
  that means for offline checks).
- **`lookback`** (optional) widens each read to
  `cursor > watermark − lookback`, the standard treatment for late-arriving
  rows. Valid only with time-typed cursors (timestamp or date).

**The duration grammar** is `<integer><unit>` with unit one of `s`/`m`/`h`/`d`
— `30m`, `2h`, `1d`. It's new to the surface (existing duration-shaped fields
like `retention_days` and `init_timeout` are unit-suffixed integers); this is
the ratified convention for future duration fields. One error per mistake,
naming the value and the fix:

```yaml
lookback: 2        # no unit
```
```
`lookback: "2"` has no unit — durations need a suffix: s, m, h, or d (e.g. `2h`).
```

```yaml
lookback: 2x        # unknown unit
```
```
`lookback: "2x"` is not a valid duration — use an integer with a unit suffix: s, m, h, or d (e.g. `30m`, `2h`, `1d`).
```

```yaml
lookback: 0h        # zero
```
```
`lookback: "0h"` is zero. Omit `lookback` to read only rows past the watermark, or give a non-zero window (e.g. `2h`).
```

**Validation splits across two phases**, and it's worth knowing which one
you're in when a cell fails: identifier shape (`cursor` must be a bare
column name, `[A-Za-z_][A-Za-z0-9_]*` — no dots, quotes, expressions) and
`lookback` parsing fail at **resolve time**, offline, before anything touches
the warehouse. Cursor **existence, type, and nullability** can only be
checked at **bind time**, against the live column — `datamk verify` is not a
safety net for a bad cursor name or a cursor of the wrong type; you'll only
find out on `datamk run`. The bind-time errors, verbatim:

```
source 'events': incremental cursor 'updated_at' does not exist in
analytics.events. Columns available: event_id, occurred_at, region, revenue.
Set `cursor:` to one of these, or drop `incremental:` to full-scan this source.
```
```
source 'events': incremental cursor 'updated_at' is VARCHAR; a cursor must be a
timestamp, date, or integer column. Point `cursor:` at a monotonic column of
one of those types, or drop `incremental:` to full-scan this source.
```
```
source 'events': `lookback: 2h` needs a time-typed cursor, but cursor 'id' is
BIGINT. Remove `lookback` — integer cursors have no time window — or set
`cursor:` to a timestamp or date column.
```

A cursor column declared **nullable** produces a bind-time *warning*, not an
error (warehouse defaults over-declare nullability):

```
source 'events': cursor 'updated_at' is nullable — rows with a NULL updated_at
are staged once at bootstrap and never seen again (NULL is excluded by
`updated_at > watermark`). Make the column NOT NULL upstream if those rows matter.
```

A typo'd key inside the block (`incremenetal:` at the source level, or an
unknown key inside `incremental:`) is rejected rather than silently
parsed as a plain connection source — the whole point is that a typo must
not turn into "this cell full-scans forever and nobody notices":

```
unknown field `windw` in `incremental:` — expected `cursor` or `lookback`.
```
```
`incremental:` is missing required field `cursor`. Name the monotonic column
to track, e.g. `cursor: updated_at`.
```

### 2a. View-backed sources

A `connection` source pointed at a BigQuery **view, materialized view, or
external table** is auto-detected and read through the BigQuery **jobs API**
instead of the usual Storage Read API pushdown path — the Storage Read API
can only read base tables, clones, and snapshots. Nothing in `cell.yaml`
changes to opt into this; the engine classifies the object against BigQuery's
own metadata (never the DuckDB-side catalog, which misreports a view as a
base table) and routes automatically.

The cost consequence is the same trade stated in [Cost
honesty](#10-cost-honesty), sharpened for views: without `incremental:`, a
view source **fully materializes on every run** — there is no pushdown to
push into. Adding `incremental:` to a view source bakes the watermark
predicate directly into the issued GoogleSQL query, so it's the only way to
avoid a full read per run for a view source. Stated plainly:

- **Bytes *returned* to the Builder scale with the delta**, by construction —
  the predicate is part of the query DuckDB hands to `bigquery_query()`.
- **Bytes *billed* still depend on the view's own SQL.** A view that
  dedupes CDC records or joins across its base tables can still scan its
  full base table to answer even a filtered query — the predicate narrows
  what's *returned*, not necessarily what BigQuery has to *read* to compute
  the view. Measure the actual job cost for your view before assuming
  `incremental:` gives you the same savings it gives a base-table source.

## 3. The delivery contract

Incremental sources are **at-least-once**. The view a transform sees on any
given run contains every row not yet accounted for, and *possibly* rows
already seen — from the lookback window in normal operation, from
rollback-then-rerun, or from `--full-refresh`. There is exactly one
invariant a transform must uphold, and it covers all three:

> A transform consuming an incremental source must be **replay-safe**:
> re-delivering a row it has already processed must not corrupt the output.

The bootstrap run (no watermark yet) sees the whole table through the same
view a delta run sees a slice through — the same source binding works both
times, unmodified. What makes a transform replay-safe, per
[ADR 0008](adr/0008-declarative-incremental-materialization.md), is that the
engine — not the author — composes the state-transition DML around your
SELECT: `upsert`/`append` reconcile against existing state unconditionally,
so replay-safety never depends on what the SELECT yields — covered next.

## 4. Landing the delta: `materialize:`

**There is one language for transforms** ([ADR 0008](adr/0008-declarative-incremental-materialization.md)):
every file under `sql/` is a SELECT — no `CREATE`, no `MERGE`, no `INSERT`,
no hooks block, no second language. The engine composes every CREATE/MERGE/
INSERT statement around your SELECT; you never hand-write DDL/DML in a
transform file. A file that does contain hand-written DDL fails to parse the
moment the engine wraps it as a subquery — loudly, naming the rule (see
[Composition errors](#4d-composition-errors) below).

**Table = file stem, no override.** `sql/fct_events.sql` builds table
`fct_events`. One file, one table; two entries resolving the same stem is a
resolve-time error. Rename the file if you want a different table name —
there is no `table:` key to ask the engine for one.

A `transforms:` entry has two forms — a bare path and a `materialize:`
mapping — and they mean the same thing when the mapping says `replace`:

```yaml
transforms:
  - sql/stg_events.sql        # bare path = materialize: replace (the default)
  - sql: sql/fct_events.sql   # explicit mapping form
    materialize: upsert       # append | upsert | replace
    key: [event_id]           # required for append/upsert; forbidden for replace
```

**Pick a strategy — three, closed, no ordering-column config anywhere (no
`unique_by`; it was proposed as an `upsert` dedup tiebreaker and declined —
dedup is query logic, it belongs in your SELECT's `QUALIFY`, not a config
field):**

| Your table is... | `materialize:` | `key:` | Example |
|---|---|---|---|
| rebuilt from scratch every run (rollups, derived dims) | `replace` | none — forbidden | a daily rollup, a dimension table |
| an accumulator where updates matter | `upsert` | required | `cursor: updated_at`, last-delivery-wins |
| an accumulator of immutable events | `append` | required | event logs, first-write-wins, re-delivery dropped |

Every value is **replay-safe by construction, never by author discipline** —
`append`/`upsert` unconditionally (reconciled against existing state, so
safety never depends on whether the SELECT yields a delta or a complete
relation); `replace` structurally (safe only because the engine restricts
*where* it's legal — the incremental-cell gate, below — never because it
trusts the SELECT is complete).

**`materialize:` is what lands the delta.** You write a bare SELECT — the
delta, nothing else, no `CREATE`, no `INSERT`, no `ANTI JOIN`, and no
trailing `;` (the engine wraps your file's text verbatim as a parenthesized
subquery, so anything after a `SELECT` statement's natural end breaks the
wrapping — a stray `;` is the single most common way to hit this). The
engine composes and runs the DML around it — `CREATE TABLE IF NOT EXISTS` /
`MERGE` (or anti-join) for `upsert`/`append`, one `CREATE OR REPLACE TABLE`
for `replace` — inside the same transaction as every other transform, and
writes exactly what it composed to a file (§4a, the eject artifact — audit
and portability only, not a migration path). Replay-safety is guaranteed
before the first run, not verified after the fact.

```yaml
- sql: sql/fct_events.sql
  materialize: upsert
  key: [event_id]
```

- **`upsert`** — a new delivery *replaces* the stored row for its key
  (`MERGE`). Use it when `cursor: updated_at` is tracking updates and those
  updates matter.
- **`append`** — insert delta rows whose key is not already present
  (anti-join on `key`); existing rows are never touched, a re-delivered key
  is dropped. Correct for append-only sources and first-write-wins
  accumulation.
- **`replace`** — rebuild the table from scratch every run, one statement:
  `CREATE OR REPLACE TABLE "<table>" AS (<select>)`. No staging needed (there
  is no delta to evaluate once and reuse — the whole SELECT runs once,
  directly). Takes **no `key:`** — forbidden if present:

  ```
  transform 'sql/rollup.sql': `key:` is not allowed with `materialize: replace`
  — replace rebuilds the table from scratch every run, so there is nothing to
  reconcile a key against. Remove `key:`, or use `materialize: upsert`/`append`
  if you need key-based reconciliation.
  ```

  For fully-derived tables — rollups, marts, dimensions, the other half of
  every real pipeline next to the accumulators `upsert`/`append` build. With
  all three, the strategy set is closed: merge into prior state, add to it,
  or discard-and-rebuild it are the only relationships a table can have to
  its prior state.

- **Guard 4c: a `replace` model that references an incremental source by
  name is a resolve-time hard error** — before anything touches a warehouse.
  The check is a **word-boundary token scan of the model's SQL text against
  the engine-owned incremental source names** — not SQL parsing. A name is
  "found" only when it's bounded on both sides by a non-identifier character
  or the start/end of the file, so `events` matches `FROM events` and
  `FROM events e` but not `FROM fct_events` or `FROM events_fct`:

  ```
  transform 'sql/rollup.sql': materialize: replace references incremental source
  'events' — rebuilding from the delta would replace the table's history with
  just the delta (truncation). Read the accumulated table instead (an
  upsert/append model over 'events' in this cell), or change this model to
  materialize: upsert/append if it should itself accumulate. See
  docs/guides/incremental.md §4.
  ```

  **The scan is per-model, not cell-wide** — a `replace` rollup that reads an
  *accumulated* table built by an `upsert`/`append` model **in the same
  cell** is fine, and the gate stays silent for it:

  ```yaml
  sources:
    events:
      connection: crm
      table: analytics.events
      incremental:
        cursor: updated_at

  transforms:
    - sql: sql/fct_events.sql   # accumulator: reads the incremental source
      materialize: upsert
      key: [event_id]
    - sql/daily_rollup.sql      # replace: reads fct_events (an accumulated
                                 # table), NOT events directly — gate silent
  ```

  ```sql
  -- sql/daily_rollup.sql — SELECT from fct_events, not events. The word
  -- "events" never appears as a whole token in this file's text, so the
  -- scan does not fire — this replace is safe and legal.
  SELECT date_trunc('day', occurred_at) AS day, count(*) AS n
  FROM fct_events
  GROUP BY 1
  ```

  **Two honest edges, both deliberate (it's a string scan, not a parser):**
  - A source name mentioned in a `--` comment false-positives — the fix is
    to reword the comment, not the SQL. Not a bug; documented here so it
    isn't mistaken for one.
  - Indirection — a CTE aliasing the source under a different name, an
    alias built from string concatenation, anything that doesn't put the
    literal source name in the file as a token — evades the scan silently.
    The scan is deliberately naive, not a security boundary; the **shrink
    detector** (§7) is the after-the-fact backstop for whatever slips past
    it.
  - `upsert`/`append` models are **never scanned** — they're delta consumers
    by design (that's the whole point of the strategy), so a reference to
    the incremental source's name in one of them is expected, not a hazard.
  - **Split into a downstream cell** remains the other option when a
    `replace` genuinely needs to read the raw delta shape rather than an
    accumulated table: put the accumulator in one cell, have a second cell
    read that cell's published output as a `cell:` source (no
    `incremental:` there — it's a complete relation) and `replace` its
    rollup over that.
  - If incremental `Cell`/`Raw` sources ever ship (ADR 0005 deferred), the
    scanned name set must extend to cover them too — this predicate is sound
    only while `connection` sources are the engine's sole delta-producers.

- **`key:`** (`upsert`/`append` only) must be **unique in the delta** — if
  your SELECT can yield two rows for the same key in one run (a duplicate
  upstream, a fan-out join), the engine hard-errors before writing anything
  rather than silently picking one or landing both:

  ```
  transform 'sql/fct_events.sql': materialize key ["event_id"] is not unique in
  the staged delta — 2 key value(s) appear more than once (e.g. 4821 (2x)).
  Declarative materialization requires one row per key in the delta; dedupe in
  your SELECT with `QUALIFY row_number() OVER (PARTITION BY event_id ORDER BY
  <col>) = 1`, naming the column that should decide which row wins.
  ```
- **A NULL key is also a hard error** (`upsert`/`append` only), before
  anything is written — a NULL key can never be deduplicated (`NULL` never
  equals `NULL`), so it would accumulate a fresh copy every run:

  ```
  transform 'sql/fct_events.sql': materialize key column 'event_id' contains
  NULL in the staged delta (3 rows). Rows with a NULL key cannot be
  deduplicated and would accumulate a new copy every run. Make 'event_id' NOT
  NULL upstream, or filter NULLs out in the SELECT.
  ```
- **Schema drift is a hard error, not a migration — for `upsert`/`append`.**
  If your SELECT's shape changes — a new column, a dropped one, a type
  change — the engine refuses to guess, and there is no ALTER path inside
  the pipeline:

  ```
  declarative table 'fct_events': the SELECT now yields column 'carrier'
  (VARCHAR) absent from the accumulated table. Declarative materialization
  does not migrate schema in place, and there is no ALTER path inside the
  pipeline (ADR 0008 §6). Recover with `datamk run --full-refresh` to rebuild
  the table at the new shape, or use `datamk attach` for one-off
  out-of-pipeline surgery.
  ```

  `replace` has no drift to detect: it recreates the table at the SELECT's
  current shape every run, by design — a shape change is just what the next
  run builds, not an error state.
- **`--full-refresh` is the schema-migration path.** For `upsert`/`append`
  it rebuilds the table from scratch (`CREATE OR REPLACE TABLE ... AS
  SELECT * FROM <the full, unfiltered delta>`) instead of merging — the one
  place `CREATE OR REPLACE` is correct on that path, because the engine
  issues it from a full re-read, never a delta. This is also the
  delete-reconciliation lever: `upsert`/`append` alone never remove
  upstream-*deleted* rows (they simply stop being delivered), so
  "incremental plus a periodic `--full-refresh`" is the same eyes-open
  pattern §5/§8 already describe. `replace` gets **no special branch** —
  it's already a full rebuild every run, flag or not; `--full-refresh`
  changes nothing about what a `replace` entry does. There is no ALTER path
  inside the pipeline for any strategy — a genuine schema change goes
  through `--full-refresh`, or through `datamk attach` for one-off surgery
  outside the pipeline entirely (ADR 0008 §8).
- **Grain inheritance applies only to the key-bearing strategies.** An
  export whose `source` is an `upsert`/`append` table can omit `grain:`
  entirely — it inherits `key:` (row identity and the filterable query
  params both); writing `grain:` explicitly there is an *extension*, not a
  restatement, and must contain every key column (grain may be finer than
  key, never coarser), checked offline before any run. An export sourced
  from a **`replace`** table gets **no inherited grain** — there is no
  `key:` to inherit from — so declare `grain:` explicitly, or get the same
  [no-grain-backstop warning](#6-verifying-replay-safety) an export with no
  grain at all gets.
- **Stale-clobber edge, accepted (`upsert` only).** `upsert` has no ordering
  column. A re-delivered row replaces the stored row by *delivery* order,
  not by content timestamp. In normal operation this is harmless (lookback
  and monotonic-cursor re-delivery windows carry equal-or-newer data, and a
  duplicate-per-key *within* one delta fails loudly, above) — the narrow
  residual is a re-delivery that carries *staler* data than what's stored,
  which clobbers with the stale value. Conditional "newer wins" merge
  (`WHEN MATCHED AND s.updated_at > d.updated_at`) is not expressible — ADR
  0008 declines it; a fourth strategy ships only if it's a fixed DML
  template with zero query semantics, and conditional merge has query
  semantics. (`replace` has no such edge — every run is a full,
  from-scratch recomputation, not a merge, so there's no "which delivery
  wins" question to have.)

### 4a. The eject artifact: audit and portability, not a migration path

On every `materialize:` run the engine **writes the composed DML to a
file**, `.cell/materialize/<table>.sql` — the exact statements it just
executed for that table, resolved against the cell directory exactly like
the rest of `.cell/`'s generated state (the catalog file, the release
manifest). Overwritten every run; not part of the contract; not meant to be
hand-edited in place (edit the `sql:` file and let the next run regenerate
it). This is the transparency guarantee: the composed SQL is plain DuckDB,
runnable anywhere, so the abstraction is inspectable and the data layer
never depends on `datamk` to be read. **It is not a migration path** — there
is no supported way to point a `transforms:` entry at this file directly;
a bare path means `materialize: replace`, which is the wrong strategy for
an ejected `upsert`/`append` accumulator (it would rebuild from the file's
one-shot SELECT every run, discarding accumulated history — the exact
failure this ADR exists to prevent). If you need what the artifact computes
to live somewhere else permanently, that's a one-off `datamk attach`
operation (§4c below) against the lake directly, not a `cell.yaml` edit.

(Verbatim multi-line SQL in a log line proved un-greppable in practice —
that's why this is a written file, not something you scrape out of run
output. The three DML statements still get one `tracing::info!` line each,
same as before, as the audit trail for *what ran*; the file is where you go
to *get* the DML.)

The artifact is re-execution-safe — its staging statement is composed as
`CREATE OR REPLACE TEMP TABLE`, not a bare `CREATE TEMP TABLE`, specifically
so the file survives being executed twice in one connection, exactly what
`--verify-replay` does to every entry's staged statement internally. The
file's own header comment states, in place, what it does and does not carry
(the NULL-key/duplicate-key/schema-drift guards are engine-side checks, not
part of the composed DML — a copy of the three statements does not carry
them) — reading it there is more reliable than reading this guide, which can
drift out of sync with the code; treat the header as authoritative.

### 4b. Composition errors

Because the engine wraps your file's text as a subquery rather than parsing
it, a file that isn't a single bare SELECT fails at that wrap, naming the
rule:

```
transform files contain exactly one SELECT; the engine owns all
CREATE/MERGE/INSERT (ADR 0008). If this file carries hand-written DDL, keep
its SELECT and pick a materialize: strategy.
```

The single most common cause is a trailing `;` — the file's text ends the
`SELECT` and then breaks the wrapping statement's syntax immediately after.
When the file's text ends with `;`, the error names that specifically:

```
Remove the trailing semicolon: the file is wrapped as a subquery
(docs/guides/incremental.md §4).
```

### 4c. Out-of-pipeline surgery: `datamk attach`

One-off manual fixes and backfill corrections — the cases §4a explicitly
declines to serve as a migration path — happen through a database client
against the lake, not through a transform file. `datamk attach` prints the
connection SQL (catalog attach, secrets, `USE`) for the running profile so
you can open DuckDB (or DBeaver, or anything else that speaks it) directly
against the same data your cell builds:

```
datamk attach -f cell.yaml -p prod
```

This is deliberate: an out-of-pipeline fix is **visible as an intervention**
— a human ran a statement by hand against the warehouse — rather than
disguised as a model a future reader would assume is reproducible from
`cell.yaml` alone. It's also the answer to "how do I ALTER a table" now that
there's no ALTER path inside the pipeline (§4, schema drift): `--full-refresh`
handles the case where the SELECT's shape changed and you want the pipeline
to rebuild around it; `datamk attach` handles everything else — a one-time
backfill, a manual correction, a migration too big or too odd to express as
`--full-refresh`.

## 5. Bootstrap and `--full-refresh`

Two moments are always a full scan and a full bill, by design:

- **Bootstrap** — the first run against a source, or the first run after
  adding `incremental:` to one. No watermark row exists yet, so the delta is
  the whole table.
- **`datamk run --full-refresh`** — re-reads every incremental source
  unfiltered and rewrites each watermark to the fresh `max(cursor)` at
  commit. It's the recovery path for a changed cursor column, an upstream
  backfill that rewrote history behind the cursor, or a direct-attach
  `verify` failure (see [Edge cases](#8-edge-cases-stated-plainly)). It is a
  flag on `run`, never deploy config — the schedule itself never
  full-refreshes; you run it as a one-off (`kubectl create job
  --from=cronjob/… -- run --full-refresh` or locally against the same
  profile). It's a no-op (with a warning) on a cell with no incremental
  sources. Because it's the most expensive thing this engine can trigger, it
  announces itself before reading:

  ```
  full refresh: re-reading 2 incremental sources from zero, rewriting watermarks
  ```

**Bootstrap needs local disk.** Staging is not a streaming filter — the
whole delta is materialized locally (a DuckDB `TEMP TABLE`) before any
transform runs, so a bootstrap on a billion-row table lands a billion rows in
the Builder's DuckDB before the first SQL file executes. The engine sets
`temp_directory` unconditionally so a large stage spills to disk instead of
failing outright; set `DATAMK_MEMORY_LIMIT` (e.g. `DATAMK_MEMORY_LIMIT=2GB`)
to cap DuckDB's in-memory budget under a cgroup limit so the pod gets a clean
spill instead of an OOM kill.

**`--init-timeout` interaction.** `datamk deploy` runs a one-shot `datamk
run` as an Init Job and waits for it (default 300s, `--init-timeout <secs>`)
before the Server ever starts. A first deploy of a large incremental cell —
which is a bootstrap, i.e. a full scan of the whole upstream — can plausibly
exceed that default. Either raise `--init-timeout` for that one deploy, or
pre-warm the watermark with a local `datamk run` against the same profile
first (a published-mode profile bootstraps the same watermark that the
deploy's Init Job will then see as already-past).

## 6. Verifying replay-safety

```
datamk run --verify-replay
```

After transforms succeed and the snapshot commits, the engine re-executes
the transform sequence a second time against the **identical staged delta**,
inside a transaction it then rolls back, and fails the run — before publish
— if any output table's contents changed between the two passes (an exact,
order-independent multiset comparison, not a hash). Cost is one extra
*local* transform pass; the warehouse is not read again, which is why it's
cheap enough to run in CI by default. A plain `INSERT ... SELECT` over an
incremental source fails loudly on your laptop instead of silently in
production:

```
replay-unsafe transform: re-running the pipeline against the same staged delta
changed table 'fct_events' (12,340 -> 15,540 rows). Incremental sources
deliver at-least-once, so a re-delivered row must not change the output — use
an anti-join or MERGE, never `CREATE OR REPLACE`. If a transform is
intentionally non-deterministic (now(), random()), --verify-replay cannot
pass. See docs/guides/incremental.md.
```

**What it structurally cannot catch: truncation.** `CREATE OR REPLACE TABLE
... AS SELECT FROM events` is *idempotent* — the real run already replaced
the table with the delta, and the replay produces the identical delta-only
table, so a pass-vs-pass comparison passes while the table's history is
already gone. `--verify-replay` cannot see this by construction; that job
belongs to the **shrink warning** ([Observability](#7-observability)) —
when the cell has an incremental source, `run` records each existing table's
row count before transforms and warns after commit if any table shrank,
naming both counts and the likely cause:

```
table shrank during a run with incremental sources — likely cause: a
`CREATE OR REPLACE` rebuilt this table from an incremental source view
instead of merging into it (see docs/guides/incremental.md) table=fct_events before=15540 after=3200
```

It's a warning, not a gate: legitimate transforms shrink tables too (dedup,
rollups rebuilt from non-incremental sources), and attributing a table to a
source would require parsing the transform SQL, which this engine refuses to
do on principle.

**The grain backstop.** `datamk verify`'s existing grain-uniqueness check is
a real, if partial, backstop against duplication — where an export declares
a `grain:`, a duplicating transform fails verify before publish regardless
of `--verify-replay`. Where no export declares any grain at all, an
incremental source gets no backstop, and `verify` (and therefore every
`run`, which auto-verifies) warns on every invocation:

```
incremental source 'events' has no grain backstop: no export declares a grain,
so `verify` cannot catch a transform that duplicates this delta. Declare
`grain:` on the export, or gate CI with --verify-replay.
```

**Determinism caveat.** `--verify-replay` re-runs your transform SQL
verbatim. A transform that calls `now()` or `random()` (or reads anything
else that isn't a pure function of the staged delta) will differ between the
two passes and fail `--verify-replay` even though it isn't actually
replay-unsafe in the sense this feature cares about. Keep transforms
deterministic over their inputs, or don't gate that cell on
`--verify-replay`.

## 7. Observability

**`run`** logs a line per incremental source, naming what was staged and
against what mark (via `tracing`, so the exact rendering depends on your
formatter/`RUST_LOG`; with the default text formatter it looks like):

```
INFO datamk::engine: staged delta past watermark source=events staged_rows=3200 watermark=2026-07-04T10:58:00+00:00
INFO datamk::engine: staged full table (bootstrap) source=signups staged_rows=48213
```

The shrink warning (§6) and the no-grain warning (§6) are ordinary `tracing`
lines on the same run.

**`status`** shows a watermark block after `LATEST` — per source, the
cursor column, the current high-water mark, and the last delta size (what
the *next* run will pick up), with bootstrap sources called out explicitly:

```
LATEST -> 7   (pointer written 2026-07-04T12:00:03Z)
watermarks (at LATEST):
  events    cursor=updated_at   mark=2026-07-04T11:58:00Z   (+3,200 rows last run)
  signups   cursor=id           absent — next run bootstraps a full scan
```

**`rollback`** surfaces the watermark move it is about to cause, so
"rollback is automatic replay" is a promise you can see, not just trust:

```
LATEST 7 -> 5
  events   watermark rewinds updated_at 2026-07-04T11:58:00Z -> 2026-07-04T09:58:00Z;
           next run re-ingests rows where updated_at > 2026-07-04T09:58:00Z
```

Both read the watermark table straight out of the published artifacts, so a
laptop with bucket credentials sees the same state the Builder wrote.

## 8. Edge cases, stated plainly

- **Deletes and hard updates behind the cursor are not captured.** A row
  deleted upstream, or updated far enough in the past that no lookback
  window reaches it, stays in the cell until a `--full-refresh`. Cursor-based
  incrementality is for append-heavy sources; CDC is a different feature and
  is not promised. "Incremental plus a periodic `--full-refresh` to
  reconcile deletes" is a legitimate pattern — it gives back part of the
  savings, with eyes open.
- **Cursor monotonicity is an author assertion the engine cannot verify.** A
  backdated `updated_at`, a non-monotonic surrogate key, or ingestion clock
  skew beyond the `lookback` window is **silent data loss**: the watermark
  only ever advances (`greatest(old, new)`), so rows that land behind it are
  never read on any subsequent run. `lookback` mitigates ordinary skew;
  choosing a truly monotonic cursor is the actual requirement.
- **NULL cursor values load once at bootstrap and never again** — the
  bind-time nullability warning (§2) names this at the moment it becomes
  knowable, but it's worth repeating here: `cursor > watermark` excludes
  NULL, permanently, for every run after the first.
- **An update-timestamp cursor with `materialize: append` drops updates.**
  `append`'s anti-join is insert-only: a re-delivered key (the same row,
  updated) is silently dropped because the key already exists in the table.
  If `cursor: updated_at` is tracking updates and those updates matter, use
  `materialize: upsert` instead (§4) — it's the single most likely mistake
  this feature invites, which is why picking the wrong strategy here is a
  choice you make explicitly in `cell.yaml`, not a DML bug to catch in review.
- **Direct-attach asymmetry.** In published mode, a `verify` failure aborts
  before the artifact is ever uploaded, so a bad snapshot *and its advanced
  watermark* die in scratch — nothing durable changed. In direct-attach mode
  (a local `catalog:` in the profile), the `COMMIT` is already durable by the
  time `verify` runs, so a `verify` failure leaves both the bad output *and*
  the advanced watermark committed — the next run will **not** re-deliver
  the rows the broken transform mangled, because the watermark already moved
  past them. The remedy is `datamk run --full-refresh` after you've fixed
  the transform.

## 9. Where the watermark lives

The watermark is stored **inside the artifact** — an engine-owned table,
`__datamk_watermarks`, inside the same DuckLake catalog as your data, not a
sidecar file next to `LATEST`. Three things fall out of that:

- **Atomic with the data.** The mark commits in the same snapshot as the
  rows it accounts for — no crash window where data lands but the mark
  doesn't (or the reverse, which would be silent data loss on the next run).
- **Rollback is automatic replay.** `datamk rollback` repoints `LATEST`; the
  watermark inside the rolled-back-to artifact is the one that matches its
  data, so the next run re-ingests exactly the rows the rollback discarded.
  No manual reconciliation step.
- **Local dev works identically.** Direct-attach mode (a `catalog:` in the
  profile) gets the same table in the same catalog — incrementality is not
  coupled to published mode or to deploy.

`__datamk_` is a **reserved, enforced** namespace, not an advisory README
line: `datamk verify` fails the run — before publish — if the transforms
committed any table matching `__datamk_%` other than `__datamk_watermarks`
itself:

```
verify: table '__datamk_junk' uses the reserved `__datamk_` prefix, which is
engine-owned (watermarks and future bookkeeping). Rename it — only
`__datamk_watermarks` may use this prefix.
```

The table is invisible to `interface:` and to everything `verify` checks
against your declared contract — it's bookkeeping, not an export. An ad-hoc
consumer browsing the raw catalog will see it; that's expected, not a leak.

## 10. Cost honesty

The whole cost argument for this feature — "an hourly cell over a large
table stops costing a full scan every hour" — depends on **cursor-predicate
pushdown actually happening in the warehouse scanner extension**. The staged
`WHERE cursor > watermark` predicate is designed to push into the scanner
(the same pushdown argument ADR 0003 makes for ordinary filters), but this
repo has **not yet proven it with a bytes-scanned assertion** against a real
warehouse (that gated integration test is still pending — see ADR 0005's
Consequences). Until that test exists and passes for a given connector, do
not assume the delta-read cost model for that connector. Worst case, the
predicate doesn't push, the scanner reads the full table anyway, and
"incremental" degrades to full-scan-then-stage-locally — strictly worse than
a plain full rebuild, because you pay the scan *and* the local staging cost.
If you're evaluating this feature for cost reasons on a connector without a
proven pushdown test, measure it yourself before you rely on it.
