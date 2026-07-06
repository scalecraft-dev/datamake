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

## 3. The delivery contract

Incremental sources are **at-least-once**. The view a transform sees on any
given run contains every row not yet accounted for, and *possibly* rows
already seen — from the lookback window in normal operation, from
rollback-then-rerun, or from `--full-refresh`. There is exactly one
invariant a transform must uphold, and it covers all three:

> A transform consuming an incremental source must be **replay-safe**:
> re-delivering a row it has already processed must not corrupt the output.

Nothing here is templated or engine-owned. The bootstrap run (no watermark
yet) sees the whole table through the same view a delta run sees a slice
through — the same SQL file runs both times, unmodified.

## 4. Replay-safe patterns

**Anti-join (insert-only).** The portable, always-available pattern:

```sql
-- transforms/02_fct_events.sql
CREATE TABLE IF NOT EXISTS fct_events (
  event_id BIGINT, occurred_at TIMESTAMP, region VARCHAR, revenue DECIMAL
);
INSERT INTO fct_events
SELECT event_id, occurred_at, region, revenue
FROM events e
ANTI JOIN fct_events f USING (event_id);
```

**Know which pattern you need.** The anti-join above is *insert-only*: an
**updated** upstream row — same `event_id`, new `updated_at` — is selected by
the cursor and then **discarded** by the anti-join, because `event_id`
already exists in `fct_events`. If `cursor: updated_at` is tracking updates
and those updates matter, the anti-join is the wrong tool. Use `MERGE INTO`
(where your pinned DuckDB/DuckLake version supports it) or delete-then-insert
on the key instead:

```sql
-- replay-safe when updates matter: delete-then-insert on the key
DELETE FROM fct_events WHERE event_id IN (SELECT event_id FROM events);
INSERT INTO fct_events SELECT event_id, occurred_at, region, revenue FROM events;
```

`cursor: updated_at` + anti-join is the plausible *wrong default* — it looks
correct (it compiles, it runs, bootstrap and single-run local dev both look
fine) and silently drops every update once it starts running against real
lookback deltas in production.

**The prohibition, stated as plainly as it can be:**

> **Never `CREATE OR REPLACE` a table fed by an incremental source.** It
> silently replaces history with just the last delta.

This is exactly the shape `datamk init` teaches as house style for
full-rebuild cells (`CREATE OR REPLACE TABLE ... AS SELECT ...` in the
scaffolded transforms) — it is the right pattern for a source with no
`incremental:` block, and the wrong one the moment a source it reads from
gains one. If you add `incremental:` to a source, check every downstream
transform that reads it for a `CREATE OR REPLACE` sitting on top of that
history.

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
pass. See docs/incremental.md.
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
instead of merging into it (see docs: incremental) table=fct_events before=15540 after=3200
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
- **An update-timestamp cursor with an anti-join transform drops updates.**
  Covered in [Replay-safe patterns](#4-replay-safe-patterns); it's listed
  here too because it's the single most likely mistake this feature invites.
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
