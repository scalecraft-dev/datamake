# ADR 0005 — Incremental source loading: watermarked executions

- **Status:** Proposed — §2's boundary (loading vs. transformation) is the
  explicit open question for team review; §1 is proposed for adoption as
  written
- **Date:** 2026-07-04
- **Deciders:** Datamake team
- **Author:** @scottypate
- **Depends on:** ADR 0003 (connection sources), ADR 0004 (versions,
  executions, published artifacts)

## Scope

This ADR adds **incremental source loading** to scheduled executions: a
`connection` source can declare a cursor column, and the Builder then reads
only rows past a persisted high-water mark instead of re-scanning the whole
upstream table every run. It covers where the watermark lives (inside the
catalog artifact — the load-bearing decision), the staging mechanics, the
replay-safety invariant transforms must uphold, `--full-refresh`, and the
interaction with rollback and compaction.

It also draws — and asks the team to ratify — a **product boundary**: datamk
owns incremental *loading* (extraction state), authors own incremental
*transformation* (what the SQL does with the delta). Engine-owned
materialization ("model kinds" à la sqlmesh/dbt) is deliberately excluded
from this ADR and recorded as the alternative we are choosing not to build
yet, with a named trigger for revisiting.

## Context

### The cost problem

Under ADR 0004, an execution is a full rebuild: `engine::run` re-binds every
source as a fresh TEMP VIEW, runs the transforms in one transaction, and
publishes a new artifact. For warehouse sources ADR 0003 states the v1
posture outright: every run is a full (pushdown-filtered) scan; "incremental
reads and local caching are explicitly out of scope for v1."

For the hourly cell the deploy model is optimized for, this is the dominant
cost and it grows with the upstream table, not with the delta. A cell
sourcing a billion-row events table re-reads a billion rows every hour to
pick up the last hour's worth. Pushdown helps only when the *transform*
filters; nothing filters on "what this cell has already seen," because
nothing records it.

### What ADR 0004 already provides

The published-artifact model was explicitly shaped to leave this door open
(ADR 0004 §10 preserves snapshot history in part for "future incremental
builds"; the fresh-catalog-per-execution alternative was rejected for
resetting it). Concretely, three facts do the work:

- **The Builder starts every execution from prior state.** Append-and-
  republish means `setup` downloads the artifact `LATEST` names and attaches
  it read-write before transforms run (`src/engine/mod.rs`). A delta build
  has a warm handle to everything already loaded.
- **Transforms are one transaction** (`engine::run`), so anything committed
  alongside them — including bookkeeping — is atomic with the data.
- **Rollback repoints `LATEST`** (ADR 0004 §9), and anything stored *inside*
  the artifact travels with it.

What is missing is exactly two things: a persisted high-water mark, and a
contract for how transforms consume a partial (delta) source view.

### The identity question: loading vs. transformation

Tools like sqlmesh and dbt already solve incremental *transformation*: the
user declares a model kind (`INCREMENTAL_BY_TIME_RANGE`,
`INCREMENTAL_BY_UNIQUE_KEY`, …), writes a SELECT, and the framework owns the
materialization — it rewrites the query, manages intervals and backfills,
issues the MERGE, restates downstream models. Copying that shape into datamk
would mean engine-owned materialization config on every transform, interval
accounting, restatement semantics — a transformation framework, competing on
sqlmesh's home turf with none of its maturity.

That is not what datamk is. The product is the **cell**: a packaged
transform + contract + serving plane + deploy story. Transforms are private
SQL files executed in order — the engine has never parsed, wrapped, or
rewritten them, and `cell.yaml` has never described what a transform *does*,
only where its inputs come from and what the output must look like
(`interface:`). The asymmetry that resolves the question:

- **Extraction state is execution machinery.** What has been loaded from an
  upstream is bookkeeping of the same kind as execution numbers and the
  `LATEST` pointer — invisible to the contract, owned by the engine. No
  external tool can own it, because it lives in datamk's artifact lineage
  and must move atomically with datamk's commits and rollbacks.
- **Transformation semantics are the author's SQL.** Whether a delta is
  appended, merged on a key, or aggregated into a rollup is the transform's
  business, expressible today in plain DuckDB SQL with zero new engine
  surface.

So this ADR builds the first and declines the second. Even a team that runs
sqlmesh for heavy modeling and sources its *outputs* into a cell needs layer
one — without a watermark, datamk re-extracts the full modeled table every
execution regardless of who modeled it.

**For team discussion:** the counter-position is real and should be argued,
not strawmanned — engine-owned materialization (a `materialize:
append|merge` block that wraps a pure SELECT) would remove boilerplate,
prevent replay-safety bugs by construction, and tell the engine which tables
a transform writes (which `verify` and future lineage would love). §2 states
why we defer it and what evidence would reopen it.

## Decision

### 1. Watermarked sources: the engine's new promise

A `connection` source may declare an `incremental:` block:

```yaml
sources:
  events:                       # cell.yaml — contract, env-free as always
    connection: crm
    table: analytics.events
    incremental:
      cursor: updated_at        # monotonic column in the source table
      lookback: 2h              # optional; re-deliver a trailing window
```

- `cursor` names a column whose values only grow for new/changed rows
  (`updated_at`, an autoincrement id, an ingestion timestamp). It is a
  property of the *data*, so it is contract (`cell.yaml`), not environment.
  Supported cursor types are a closed set — timestamp, date, and integral —
  validated at bind time against the source's actual column type; anything
  else fails loud, naming the column and its type.
- `lookback` (optional, duration) widens each read to
  `cursor > watermark − lookback`, the standard treatment for late-arriving
  rows. Valid only with time-typed cursors; validated when the cursor type
  is known.
- The block is valid only on `connection` sources in this ADR. Raw and Cell
  sources are deferred (see Consequences) — the cost problem lives in
  warehouse scans, and `Source` stays `#[serde(untagged)]` with the variants
  still disjoint by key (`incremental:` rides inside the existing
  `connection` mapping; bare-string Raw sources are untouched).

**Binding an incremental source** (replacing the plain TEMP VIEW arm in
`engine::bind_source` for these sources only):

1. Read the source's watermark from `__datamk_watermarks` in the attached
   lake catalog (absent table or row ⇒ no watermark ⇒ bootstrap).
2. **Stage the delta locally**:
   `CREATE TEMP TABLE __delta_<n> AS SELECT * FROM __conn_<c>.<table>
   WHERE <cursor> > <watermark [− lookback]>` — unfiltered on bootstrap.
   The predicate pushes into the scanner (ADR 0003's inlining argument),
   so the warehouse scans the delta, not the table, and is read **exactly
   once per run** — `max(cursor)` and the transforms both read the staged
   copy, never the warehouse twice.
3. Bind the source name as a TEMP VIEW over the staged table, so transforms
   see the identical surface they see today (a view named `<source>`).
4. Compute `max(<cursor>)` from the staged delta (local, cheap).
5. Inside the transform transaction — after transforms succeed, before
   `COMMIT` — advance the watermark: `new = greatest(old, max_seen)`,
   row-per-source in `__datamk_watermarks`, no-op when the delta is empty.
   Watermarks only advance; a source that hands back older data cannot drag
   the mark backwards.

**Where the watermark lives is the design.** `__datamk_watermarks` is an
engine-owned table *inside the lake catalog* — inside the published
artifact. Three properties fall out, none of which a sidecar object next to
`LATEST` could offer:

- **Atomic with the data.** The mark commits in the same DuckLake snapshot
  as the rows it accounts for. There is no ordering to get wrong and no
  crash window where data landed but the mark didn't (or worse, the
  reverse — silent data loss on the next run).
- **Rollback is automatic replay.** `datamk rollback` repoints `LATEST`;
  the watermark inside the rolled-back-to artifact is the one that matches
  its data, so the next scheduled execution re-ingests exactly the rows the
  rollback discarded. No reconciliation step, no operator checklist item.
  (The manual-PUT escape hatch inherits this for free — one more reason
  ADR 0004 §9 was right to put state in the artifact.)
- **Local dev works identically.** Direct-attach mode (a `catalog:` in the
  profile) gets the same table in the same catalog; incrementality is not
  coupled to published mode or to deploy at all.

The table is engine-internal: never exported, invisible to `interface:`,
and `verify` ignores it. Its per-execution history is ordinary snapshot
data; ADR 0004 §10's compaction handles it with no special case.

### 2. The transform contract: at-least-once delivery, replay-safe SQL

The engine's promise is deliberately narrow: an incremental source view
contains **every row not yet accounted for, and possibly rows already
seen**. Possibly-seen rows arrive from the lookback window in normal
operation, from rollback-then-rerun, and from `--full-refresh`. One
invariant covers all three:

> **A transform consuming an incremental source must be replay-safe**:
> re-delivering a row it has already processed must not corrupt the output.

That is plain SQL, no templating and no engine wrapping — e.g.:

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

(or `MERGE INTO` where the pinned DuckDB/DuckLake version supports it; the
anti-join form is the portable floor). On bootstrap the unfiltered view
makes the same file perform the initial load; there is no separate
first-run SQL and no `is_incremental()` conditional — the *view contents*
change across runs, the SQL does not.

The engine does **not** verify replay-safety — it cannot, without parsing
transforms, which is the line this ADR refuses to cross. Two honest
mitigations: `verify`'s grain-uniqueness check already fails the execution
before publish when a non-replay-safe append duplicates rows covered by a
declared grain (a real backstop, but only where grain covers the write);
and the scaffold/docs teach the anti-join pattern next to the `incremental:`
example, stating the invariant in exactly the terms above.

**This boundary is the discussion point.** We are choosing "engine binds
deltas, author writes replay-safe DML" over "engine owns materialization."
Named trigger for revisiting: if real cells accumulate copy-pasted
anti-join/MERGE boilerplate or recurring replay-safety bugs that the grain
backstop misses, a minimal engine-wrapped mode (`materialize: append|merge`
over a pure SELECT) is the recorded next step — as an *addition* to this
contract, not a replacement; the watermark layer beneath it is identical
either way, which is why the layers are separable and §1 need not wait on
this debate.

### 3. `--full-refresh`

`datamk run --full-refresh` binds every incremental source **unfiltered**
and rewrites each watermark to the fresh `max(cursor)` at commit. Under the
replay-safety invariant this is correct by construction — a full re-read is
just the largest possible replay. It exists for cursor-column changes,
upstream backfills that rewrote history behind the cursor, and suspicion.
It is a flag on `run`, not deploy config: an operator runs it as a manual
Job (`kubectl create job --from=cronjob/…` plus the flag, or locally
against the same profile); the schedule itself never full-refreshes.
Bootstrap needs no flag — an absent watermark already means "read
everything."

### 4. What does not change

- **Scheduling.** The CronJob renders exactly as today (`render.rs`);
  cadence, `concurrencyPolicy: Forbid`, the conditional-PUT guard, and the
  publish protocol are untouched. Incrementality changes what an execution
  *reads*, not what an execution *is*.
- **Serve, release, pinning, the interface contract.** Sources remain
  session-local Builder inputs; the serving plane cannot tell an
  incremental execution from a full one, and must not be able to.
- **Non-incremental sources.** Any source without `incremental:` binds
  exactly as today. Full-rebuild cells remain the default and stay the
  right choice for small upstreams and for transforms that are cheaper to
  re-run than to make replay-safe.

## Consequences

- The hourly cell over a large warehouse table stops costing a full scan
  per hour; each execution reads one delta (plus lookback), staged locally
  once, and warehouse cost scales with change rate instead of table size.
- A new engine-owned table rides inside every artifact that uses the
  feature. It is invisible to every contract surface; the ad-hoc consumer
  (ADR 0004 §12) will see `__datamk_watermarks` when browsing — the README
  section gains one line saying what it is and that the `__datamk_` prefix
  is reserved.
- Incremental sources are **at-least-once**; deletes and hard updates
  behind the cursor are *not captured* — a row deleted upstream stays in
  the cell until a `--full-refresh`. This is documented as the model's
  edge, not hidden: cursor-based incrementality is for append-heavy
  sources; CDC is a different feature and is not promised.
- **The work, honestly (dependency order):**
  1. Schema: `Incremental { cursor, lookback }` on `Source::Connection` and
     `ResolvedSource::Connection`; resolve-time validation (identifier
     shape, lookback parse); tests for untagged-enum stability.
  2. Engine: watermark read/create in the attached catalog; the staged-
     delta bind path in `bind_source` (TEMP TABLE + view over it); cursor
     type check; `greatest`-advance write inside the transform transaction;
     `--full-refresh` through `cli.rs` into `run`.
  3. `datamk status`: print each incremental source's watermark next to
     the published range — the read side that makes replay and rollback
     legible from a laptop with bucket credentials.
  4. Scaffold/docs: commented `incremental:` block in the `init` cell
     template; the replay-safe anti-join pattern in the scaffolded
     transform comments; README edge-cases paragraph (deletes, lookback,
     full-refresh).
  5. e2e: two-execution kind/MinIO run asserting the second execution's
     staged delta excludes the first's rows and that rollback-then-run
     re-delivers them; a BigQuery integration test gated on credentials
     asserting pushdown of the cursor predicate (the sharpest external
     assumption — the community extension must actually push the filter,
     or "incremental" quietly degrades to full-scan-then-local-filter;
     prove it where it runs, per the ADR 0004 house rule).
- Nothing renders differently at deploy; no overlay or profile field is
  added. The feature is engine-level and works identically in local
  direct-attach mode.

## Alternatives considered

- **Engine-owned materialization (model kinds — the sqlmesh/dbt shape).**
  A `materialize:` block per transform (append/merge + key) over pure
  SELECTs, with the engine issuing DDL/DML. Deferred, not rejected — §2
  records the trigger for revisiting. Deferred because it converts datamk's
  transform model from "your SQL, executed" into a framework contract the
  engine must own forever (query wrapping, mode semantics, backfill and
  restatement questions arrive immediately behind it), because it is
  separable from — and strictly on top of — the watermark layer, and
  because building a worse sqlmesh is a losing position while "the cell
  with a contract and a serving plane" is not a thing sqlmesh does.
- **Delegate incrementality entirely to sqlmesh/dbt upstream** (datamk
  stays full-refresh; heavy modeling happens in a framework whose outputs
  a cell sources). Rejected as a *substitute*: it composes with this ADR
  rather than replacing it — even a perfectly incremental upstream model is
  re-extracted in full every execution without a datamk-side watermark.
  The extraction state cannot be outsourced; it must live and roll back
  with the artifact lineage.
- **Watermark as a sidecar object next to `LATEST`** (e.g.
  `catalog/WATERMARKS`). Rejected: it splits one logical commit across two
  non-atomic writes (snapshot, then pointer-adjacent PUT), creating the
  exact crash windows §1 eliminates, and every rollback would need a
  manual watermark reconciliation step that operators would eventually
  skip.
- **Wall-clock watermarks** (record the run's start time, filter
  `cursor > last_run_at`). Rejected: it silently assumes the cursor is a
  timestamp in a clock synchronized with the Builder's; warehouse ingestion
  lag or skew becomes silent data loss. The mark must come from the data
  (`max(cursor)` of rows actually staged), never from the clock.
- **Filter in the view, no local staging** (bind the TEMP VIEW with the
  watermark predicate and let transforms hit the warehouse directly).
  Rejected: `max(cursor)` and each transform referencing the source would
  each re-scan the warehouse; staging reads the delta exactly once and
  makes the accounting (`max` of what was *actually delivered*) exact.
- **Templating/conditionals in transform SQL** (`is_incremental()` à la
  dbt). Rejected: datamk transforms are plain SQL files by design; the
  bootstrap case is already handled by the unfiltered first read, so the
  conditional buys nothing but a template language.
- **Incremental Cell and Raw sources in this ADR.** Deferred. Cell sources
  have a better primitive available — DuckLake snapshot diffing against
  the upstream artifact (read only rows added since snapshot S), which
  deserves its own design; Raw sources want file-listing state (new
  objects under a prefix), a different mechanism entirely. Both slot into
  the same `__datamk_watermarks` table when they come.
- **CDC (change data capture) for deletes/updates.** Out of scope and
  explicitly not promised; the honest edge is documented instead. If a
  connector someday exposes a change stream, it arrives as a new source
  capability, not a stretch of cursor semantics.
