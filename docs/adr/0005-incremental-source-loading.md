# ADR 0005 — Incremental source loading: watermarked executions

- **Status:** Proposed — revised 2026-07-04 after team review (architecture,
  engineering, product, DX). §2's boundary, previously left as an open
  question, is now argued from principle and proposed for adoption; the
  review's required guardrails are folded into §§1–2 and 4. Revised
  2026-07-05 during implementation (see §§1–2 for the two corrections).
- **Date:** 2026-07-04 (revised 2026-07-05)
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
replay-safety invariant transforms must uphold **and the behavioral
verification that enforces it**, `--full-refresh`, the observability that
makes the state legible, and the interaction with rollback and compaction.

It also draws a **product boundary** and states it as principle:

> **Abstract the state, never the query.**

datamk owns incremental *loading* (extraction state); authors own
incremental *transformation* (what the SQL does with the delta). Engine-owned
materialization ("model kinds" à la sqlmesh/dbt) is **declined**, not merely
deferred — §2 argues why the query-owning half of that abstraction is a
hindrance by construction, names the one capability we honestly give up by
declining it, and states what evidence would reopen the question.

**Sequencing:** this ADR depends on ADRs 0003 and 0004, both still Proposed.
It is written now because it validates 0004's artifact model (which preserved
snapshot history explicitly for this payoff), but it queues **behind** them
in build order — incremental loading must not jump the connectors it
optimizes.

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

### The identity question: abstract the state, never the query

Tools like sqlmesh and dbt already solve incremental *transformation*: the
user declares a model kind (`INCREMENTAL_BY_TIME_RANGE`,
`INCREMENTAL_BY_UNIQUE_KEY`, …), writes a SELECT, and the framework owns the
materialization. The honest steelman for that shape — stated so we are not
strawmanning what we decline — is that these tools are not abstracting SQL
semantics; they are abstracting **state across runs**. A single SQL file
cannot express "which intervals have I processed," "backfill March only,"
or "restate everything downstream of this model." That statefulness is
real, and it is not in the SQL standard.

But those tools take the query along with the state. The framework rewrites
your SELECT, wraps it in its materialization, and decides the DML — and once
a tool owns the query, it has a ceiling: the framework author's imagination,
not the engine's capability. When a use case needs a specific physical
strategy — a partition swap instead of a MERGE, an engine-specific
`INSERT OR REPLACE`, an anti-join tuned to a key distribution, a two-stage
dedupe — the author fights the query generator instead of writing the four
lines of SQL they already know. Jinja-templated SQL with `is_incremental()`
branches is the visible symptom: a template language reinventing control
flow that SQL never needed, because the tool inserted itself between the
author and the engine. SQL semantics are already well known and understood;
an abstraction over them subtracts capability rather than adding it.

The correct decomposition is the other one: **own the state, never the
query.** The engine holds the minimum viable state — a high-water mark,
atomic with the data it accounts for — and hands the author an honest view.
The SQL stays the author's, all the way down to the engine, with the full
surface of the dialect available for whatever physical strategy the case
demands. This is also consistent with everything datamk already is: the
product is the **cell** — a packaged transform + contract + serving plane +
deploy story. Transforms are private SQL files executed in order; the engine
has never parsed, wrapped, or rewritten them, and `cell.yaml` has never
described what a transform *does*, only where its inputs come from and what
the output must look like (`interface:`).

Two facts complete the argument:

- **Extraction state is execution machinery.** What has been loaded from an
  upstream is bookkeeping of the same kind as execution numbers and the
  `LATEST` pointer — invisible to the contract, owned by the engine. No
  external tool can own it, because it lives in datamk's artifact lineage
  and must move atomically with datamk's commits and rollbacks. Even a team
  that runs sqlmesh for heavy modeling and sources its *outputs* into a cell
  needs this layer — without a watermark, datamk re-extracts the full
  modeled table every execution regardless of who modeled it.
- **Transformation semantics are the author's SQL.** Whether a delta is
  appended, merged on a key, or aggregated into a rollup is the transform's
  business, expressible today in plain DuckDB SQL with zero new engine
  surface.

Declining the query-owning abstraction has two honest costs, both owned in
this ADR rather than hidden: we give up **declarative partial backfill**
(named in Alternatives — `--full-refresh` is our only, coarse, lever), and
we give up **correct-by-construction replay safety**, which we replace with
**correct-by-verification** (§2's `--verify-replay`). Every enforcement
mechanism in this ADR observes what the SQL *did*, never what it *says* —
the engine verifies behavior and refuses, on principle, to parse queries.

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
  validated at bind time against the source's actual column type.
- `lookback` (optional) widens each read to
  `cursor > watermark − lookback`, the standard treatment for late-arriving
  rows. Valid only with time-typed cursors; validated when the cursor type
  is known. **Format note:** `2h`-style duration strings are new to the
  surface (existing fields use unit-suffixed integers: `retention_days`,
  `init_timeout` seconds). This ADR deliberately ratifies duration strings
  (`30m`, `2h`, `1d`) as the convention for future duration-valued fields,
  because lookback windows genuinely span mixed units; the parser and its
  error messages are part of work item 1.
- The block is valid only on `connection` sources in this ADR. Raw and Cell
  sources are deferred (see Alternatives) — the cost problem lives in
  warehouse scans. `Source`'s variants stay disjoint by key, but the enum
  drops `#[serde(untagged)]` for a manual key-dispatch deserializer — the
  hardening note below explains why untagged cannot deliver this section's
  error-text requirement.

**Schema hardening (required).** An untagged `Source` enum does not deny
unknown fields, so a typo'd key (`incremenetal:`) would deserialize cleanly
as a plain connection source — **silently running full scans forever while
the author believes the cell is incremental**. Closing that with
`deny_unknown_fields` inside `#[serde(untagged)]` does not work: untagged
deserialization discards inner-variant errors and reports only "data did
not match any variant", so the typo hazard and readable error text are
unsatisfiable together under untagged. `Source` therefore deserializes
through a manual impl that dispatches on key presence (string ⇒ raw;
`cell` ⇒ cell; `connection` ⇒ connection; both or neither ⇒ a named
error), with `deny_unknown_fields` helper structs per mapping variant so
field-level errors surface verbatim — a deliberate leniency tightening:
stray keys on cell/connection sources that previously parsed silently now
error. Work item 1's tests assert the *error text* a user sees on a
malformed block (missing `cursor`, unknown key, unparseable `lookback`),
not merely that the happy path parses — error-message quality is the
point; variant disambiguation was never the hard part.

**Validation and errors.** Validation splits across two phases, and the ADR
states the split so authors know what `verify` can and cannot catch offline:
identifier shape and `lookback` parsing fail at **resolve time** (offline);
cursor existence, type, and nullability can only fail at **bind time**
against the live warehouse — offline `verify` is *not* a safety net for a
bad cursor. Errors follow the house standard (name the value, the actual
state, and the fix):

```
source 'events': incremental cursor 'updated_at' not found in
analytics.events. Columns available: event_id, occurred_at, region, revenue.
```
```
source 'events': incremental cursor 'updated_at' is VARCHAR; cursors must be
timestamp, date, or integer. Point `cursor:` at a monotonic column of one of
those types, or drop `incremental:` to full-scan this source.
```
```
source 'events': `lookback: 2h` needs a time-typed cursor, but cursor 'id'
is BIGINT. Remove `lookback` (integer cursors have no time window), or set
`cursor:` to a timestamp column.
```

A cursor column declared nullable produces a bind-time **warning** (not an
error — warehouse defaults over-declare nullability): rows with a NULL
cursor are staged once at bootstrap and **never seen again**, because
`cursor > watermark` excludes NULL. The warning names that drop; the docs
state it as an edge alongside deletes.

**Binding an incremental source** (replacing the plain TEMP VIEW arm in
`engine::bind_source` for these sources only):

1. Check for `__datamk_watermarks` via catalog metadata and read the
   source's row if present (absent table or row ⇒ no watermark ⇒
   bootstrap). The read is against the **current** table state, never a
   version-pinned view. **No watermark DDL executes here** — `bind_source`
   runs before `BEGIN`, and any auto-committed statement would emit a second
   DuckLake snapshot and break the one-snapshot-per-execution invariant this
   engine is built on. Table creation is deferred to step 5.
2. **Stage the delta locally**:
   `CREATE TEMP TABLE __delta_<n> AS SELECT * FROM __conn_<c>.<table>
   WHERE <cursor> > <watermark [− lookback]>` — unfiltered on bootstrap.
   The generated SQL follows the file's existing hygiene: the cursor
   identifier is double-quoted at the build site (same discipline as
   `qualify()`), and the watermark literal is rendered **typed** per cursor
   type (timestamp/date/integer literal syntax), never string-spliced —
   resolve-time shape validation is defense in depth, not a substitute.
   The predicate pushes into the scanner (ADR 0003's inlining argument), so
   the warehouse scans the delta, not the table, and is read **exactly once
   per run** — `max(cursor)` and the transforms both read the staged copy,
   never the warehouse twice.
3. Bind the source name as a TEMP VIEW over the staged table, so transforms
   see the identical surface they see today (a view named `<source>`).
4. Compute `max(<cursor>)` from the staged delta (local, cheap).
   `bind_source` **returns the per-source advance to `run`** — this is a
   signature change (today it returns `()`), because the value is computed
   pre-transaction but written inside it; `run` threads the collected
   advances across that boundary.
5. Inside the transform transaction — after transforms succeed, before
   `COMMIT` — persist the state: `CREATE TABLE IF NOT EXISTS
   __datamk_watermarks` **inside the transaction** (first incremental run
   only), then an in-place upsert of one row per source with
   `new = greatest(old, max_seen)`. **When the delta is empty the write is
   skipped entirely** — `max(cursor)` over zero rows is NULL, and the guard
   is explicit so no `greatest(old, NULL)` evaluation can ever erase a
   watermark; a unit test pins this. Watermarks only advance; a source that
   hands back older data cannot drag the mark backwards.

**Watermark storage is typed.** `__datamk_watermarks` carries one row per
source with per-type value columns (e.g. `mark_ts TIMESTAMPTZ`,
`mark_date DATE`, `mark_int BIGINT` — exactly one non-NULL), so no watermark
value ever round-trips through a string. The remaining round-trip — a
DuckDB-read `max(cursor)` rendered into a predicate the *warehouse*
evaluates next run — must preserve exact ordering or the gap is silent
loss; the gated warehouse test asserts no-gap across that boundary with a
timezone-carrying cursor (see Consequences).

**Where the watermark lives is the design.** `__datamk_watermarks` is an
engine-owned table *inside the lake catalog* — inside the published
artifact. Three properties fall out, none of which a sidecar object next to
`LATEST` could offer:

- **Atomic with the data.** The mark commits in the same DuckLake snapshot
  as the rows it accounts for. There is no ordering to get wrong and no
  crash window where data landed but the mark didn't (or worse, the
  reverse — silent data loss on the next run). In published mode the
  durable commit boundary is the artifact upload, not the local `COMMIT`:
  a crash anywhere before `publish_execution` dies in the throwaway scratch
  catalog with `LATEST` untouched, and a crash between the artifact PUT and
  the `LATEST` PUT orphans an artifact whose mark is never read (ADR 0004
  §4 semantics).
- **Rollback is automatic replay.** `datamk rollback` repoints `LATEST`;
  the watermark inside the rolled-back-to artifact is the one that matches
  its data, so the next scheduled execution re-ingests exactly the rows the
  rollback discarded. No reconciliation step, no operator checklist item.
  (The manual-PUT escape hatch inherits this for free — one more reason
  ADR 0004 §9 was right to put state in the artifact.)
- **Local dev works identically.** Direct-attach mode (a `catalog:` in the
  profile) gets the same table in the same catalog; incrementality is not
  coupled to published mode or to deploy at all.

**Atomicity is not concurrency, and this ADR does not claim it is.** Two
concurrent executions both starting from `LATEST = N` would each stage the
same delta and publish sibling artifacts; the conditional PUT makes them
distinct executions, not corrupt ones, and `LATEST` ends at whichever
pointer write landed last — exactly ADR 0004 §5's dead-branch semantics.
Concurrency safety comes from `concurrencyPolicy: Forbid` on the schedule
plus those semantics, at the same safety level as the data itself.

**The staging trade, stated honestly.** Staging buys read-the-warehouse-
exactly-once and exact accounting (`max` of rows *actually delivered*), and
it costs two things the original draft did not say: a transform's own
predicates no longer push to the warehouse (they filter the already-landed
local copy — the right trade for a delta consumed whole, irrelevant for
small deltas, real for transforms that filter a large delta hard), and the
delta is **fully materialized locally** before any transform runs. On
bootstrap that is the entire upstream table landing in the Builder's DuckDB.
Work item 2 therefore includes spill configuration (`temp_directory` +
memory limit on the engine connection) so a large bootstrap spills to disk
instead of OOM-ing the Builder pod, and the docs must flag the deploy
interaction: a first deploy of a large incremental cell will plausibly
exceed `--init-timeout`'s 300s default — bootstrap with a raised timeout or
pre-warm with a local `run` against the same profile first.

**Direct-attach asymmetry.** In published mode a `verify` failure aborts
before upload, so a bad snapshot *and its advanced watermark* die in
scratch. In direct-attach mode the `COMMIT` is already durable when `verify`
runs, so a verify failure leaves both the (bad) output and the advanced
watermark committed — the next run will *not* re-deliver the delta the
broken transform mangled. This asymmetry predates this ADR (verify-after-
commit is the existing engine shape); what is new is that it now covers
bookkeeping too. The documented remedy is `--full-refresh` after fixing the
transform (correct by construction under §2's invariant); moving `verify`
inside the transaction is noted as a possible future engine change and is
out of scope here.

The table is engine-internal: never exported, invisible to `interface:`,
and `verify` ignores it. **The `__datamk_` prefix is a reserved namespace,
enforced, not advisory**: `verify` fails the execution before publish if
the transforms committed any table matching `__datamk_%` other than the
watermark table — a reserved prefix guarded only by a README line is not
reserved. The rule is stated generically so future engine tables inherit
it. The watermark row's per-execution history is ordinary snapshot data;
ADR 0004 §10's compaction handles it with no special case (`select_
expirable` never expires the newest snapshot, where the current mark
lives), and the e2e suite exercises expire/cleanup against a
watermark-bearing lineage rather than assuming it. One operational note:
each advance writes a small Parquet file into the artifact's data path —
ordinary DuckLake behavior, folded into the run's snapshot and cleaned by
the existing `ducklake_cleanup_old_files`; noted here so the file spray is
not mistaken for a bug.

### 2. The transform contract: at-least-once delivery, replay-safe SQL, verified behaviorally

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

**Know which pattern you need.** The anti-join above is insert-only: an
*updated* upstream row (same `event_id`, new `updated_at`) is selected by
the cursor and then **discarded** by the anti-join. If the cursor is an
update timestamp and updates matter, the replay-safe pattern is `MERGE` (or
delete-then-insert on the key), not the anti-join. The docs and scaffold
present both patterns with exactly this contrast, because
`cursor: updated_at` + anti-join is the plausible wrong default.

**The engine does not parse transforms — it verifies their behavior.** The
invariant is unverifiable syntactically without crossing the line this ADR
refuses to cross, and the danger is specifically that **the dev loop never
replays**: a non-replay-safe transform works perfectly on bootstrap and in
single-run local dev, then silently corrupts data in production every hour
via the lookback window. Two failure shapes exist, and the second is worse
than the one the original draft named:

- **Duplication**: a plain `INSERT ... SELECT FROM events` re-inserts
  lookback rows every run. The grain-uniqueness check catches this before
  publish *where a grain covers the written table* — a real backstop, but
  partial.
- **Truncation**: `CREATE OR REPLACE TABLE ... AS SELECT FROM events` —
  the exact shape the `init` scaffold teaches as house style for
  full-rebuild cells — silently **replaces the whole table with just the
  delta**. Data loss, not duplication, and the grain backstop cannot see it
  (a delta is perfectly unique on grain). **Neither can `--verify-replay`**:
  a destructive rebuild is idempotent — the real run already replaced the
  table with the delta, and the replay produces the identical delta-only
  table — so a pass-vs-pass comparison passes while history is already
  gone. Truncation is therefore *warned*, not gated (enforcement item 2).

The enforcement, all behavioral, none of it query-parsing:

1. **`datamk run --verify-replay`** — the primary guard **for duplication**
   (and any other non-idempotent replay effect). After transforms succeed
   and the snapshot commits, the engine re-executes the transform sequence
   against the **identical staged delta** inside a transaction it then
   rolls back — DuckLake rolls back DDL and data cleanly with no residual
   snapshot, pinned by `ducklake_rollback_restores_committed_state_and_
   snapshots` in `src/engine/mod.rs` — and fails the execution before
   publish if any output table's contents changed between passes (exact
   order-independent multiset comparison, not a hash). A duplicating
   transform — a plain `INSERT ... SELECT` — fails loudly on the author's
   laptop instead of silently in production. Truncation it structurally
   cannot see (idempotent, per the failure shapes above); item 2 covers it.
   Cost is one extra *local* transform pass; the warehouse is not re-read
   (the delta is already staged), which is why this is cheap enough to run
   by default in CI and in the e2e suite. Attribution is per-table (the
   engine does not parse transforms, so it cannot name the offending file);
   the failure message names the invariant and the fix:
   ```
   replay-unsafe transform: re-running the pipeline against the same staged
   delta changed table 'fct_events' (12,340 -> 15,540 rows). Incremental
   sources deliver at-least-once; use an anti-join or MERGE, never
   `CREATE OR REPLACE` (see docs: incremental).
   ```
2. **The shrink warning** — the truncation detector. When the cell has an
   incremental source, `run` records each existing lake table's row count
   before transforms and, after commit, warns if any table shrank, naming
   both counts and the likely cause (a `CREATE OR REPLACE` over an
   incremental source view). A warning rather than a gate: legitimate
   transforms shrink tables (dedup, rollups rebuilt from non-incremental
   sources), and attributing tables to sources would require parsing SQL,
   which this ADR refuses. It converts the worst silent failure into a
   per-run, named signal.
3. **Resolve-time warning when the backstop is off**: an `incremental:`
   source in a cell where no export declares a grain warns on every run —
   ```
   warning: source 'events' is incremental but no export declares a grain;
   `verify` cannot backstop replay-safety here. Declare `grain:` on the
   exported table so duplicate deltas are caught before publish.
   ```
4. **Row counts as a standing detector**: `run` prints each incremental
   source's staged row count (free — the delta is a local table), and
   `status` shows the last delta per source (§4). Duplication makes output
   counts climb run-over-run; truncation collapses them to delta size. A
   number the operator sees every run beats a doc paragraph read once.
5. **The grain-violation error names the likely cause** when the violating
   table is downstream of an incremental source: "…grain uniqueness
   violated — if this table consumes an incremental source, the transform
   is likely not replay-safe (see docs: incremental)."
6. **The scaffold ships the safe pattern as live SQL, not a comment.** When
   `init` scaffolds (or docs describe) an incremental source, the paired
   transform *is* a working anti-join the author edits — the path of least
   resistance is the safe path — with the MERGE variant and this warning
   alongside: **never `CREATE OR REPLACE` a table fed by an incremental
   source; it silently replaces history with the last delta.**

**Declined alternatives for this guardrail** (argued in Alternatives): a
`replay_safe: true` attestation field (ceremony without verification —
collecting promises instead of checking behavior is the failure mode we
just declined at the query level) and rejecting grain-less incremental
cells outright (too coarse; the warning plus `--verify-replay` covers it
without a false constraint).

**Reopening condition, retuned.** The original draft recorded engine-owned
materialization as "the next step if boilerplate or replay bugs
accumulate." Under this ADR's principle that is the wrong remedy on both
counts: the answer to accumulated boilerplate is better scaffolds, and the
answer to replay bugs is the verification above — both of which observe
behavior without owning the query. Materialization is reopened only by
evidence that **behavioral verification cannot hold the invariant in
principle** — a class of replay unsafety that no behavioral check, gate or
warning, can surface. (Truncation is a known instance of what the replay
comparison alone cannot see; it is covered behaviorally by the shrink
warning, so it does not trip this condition.) Not by incident counts —
which would mean collecting
evidence from already-burned users — and not by boilerplate volume. The
watermark layer beneath is identical either way, which is why §1 does not
wait on any future revisiting.

### 3. `--full-refresh`

`datamk run --full-refresh` binds every incremental source **unfiltered**
and rewrites each watermark to the fresh `max(cursor)` at commit. Under the
replay-safety invariant this is correct by construction — a full re-read is
just the largest possible replay. It exists for cursor-column changes,
upstream backfills that rewrote history behind the cursor, recovery from a
direct-attach verify failure (§1), and suspicion. It is a flag on `run`,
not deploy config: an operator runs it as a manual Job
(`kubectl create job --from=cronjob/…` plus the flag, or locally against
the same profile); the schedule itself never full-refreshes. Bootstrap
needs no flag — an absent watermark already means "read everything."

Because this is the most expensive operation the feature can trigger, it is
not silent. The flag carries full help text (when to use it, that the
schedule never does, that it is a no-op without incremental sources), and
the run announces itself before reading:

```
full refresh: re-reading 2 incremental sources from zero, rewriting watermarks
```

The docs state the cost plainly: on a large table a full refresh is a full
scan and a full bill — "incremental plus periodic full-refresh to reconcile
deletes" is a legitimate pattern, chosen with eyes open, that gives back
part of the savings.

### 4. Making the state legible

The watermark is invisible state unless the surface narrates it; these are
part of the feature, not follow-ups.

**`run`** prints per incremental source what was staged and against what
mark:

```
events: staged 3,200 rows past watermark 2026-07-04T10:58Z
```

**`status`** gains a watermark block after `LATEST`, showing per source the
cursor column, the current high-water value, and the last delta size — what
the next run will pick up — with the bootstrap state explicit:

```
LATEST -> 7   (pointer written 2026-07-04T12:00:03Z)
watermarks (at LATEST):
  events    cursor=updated_at   mark=2026-07-04T11:58:00Z   (+3,200 rows last run)
  signups   cursor=id           absent — next run bootstraps a full scan
```

**`rollback`** surfaces the watermark move it is about to cause — otherwise
"rollback is automatic replay" is a promise the operator executing it never
sees:

```
LATEST 7 -> 5
  events   watermark rewinds updated_at 2026-07-04T11:58:00Z -> 2026-07-04T09:58:00Z;
           next run re-ingests rows where updated_at > 2026-07-04T09:58:00Z
```

### 5. What does not change

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
- **The engine's relationship to transform SQL.** It executes it. It never
  parses, wraps, or rewrites it — §2's verification observes outputs, not
  queries.

## Consequences

- The hourly cell over a large warehouse table stops costing a full scan
  per hour; each execution reads one delta (plus lookback), staged locally
  once, and warehouse cost scales with change rate instead of table size.
  **Shipping precondition:** this claim is contingent on cursor-predicate
  pushdown actually occurring in the community scanner extensions. The
  gated warehouse test (below) must prove it via a **bytes-scanned
  assertion** before the feature is documented or marketed as cost
  reduction — if the predicate does not push, "incremental" degrades to
  full-scan-then-local-stage, which is *worse* than today, and the feature
  does not ship for that connector until it pushes or the degradation is
  loudly documented per connector. (For **view-backed sources** read via
  the jobs API, this precondition cannot be satisfied by construction:
  the predicate is evaluated server-side, so bytes *returned* scale with
  the delta, but bytes *scanned/billed* depend on the view's own SQL —
  ADR 0006 documents that degradation per this clause.)
- Two moments still cost a full scan by design and are labeled as such in
  docs: **bootstrap** (first run, or first run after adding
  `incremental:`) and every **`--full-refresh`**. Neither is a surprise
  bill if the docs do their job.
- A new engine-owned table rides inside every artifact that uses the
  feature. It is invisible to every contract surface; the ad-hoc consumer
  (ADR 0004 §12) will see `__datamk_watermarks` when browsing — the README
  states the `__datamk_` prefix rule, and `verify` enforces it (§1).
- Incremental sources are **at-least-once**; the documented edges, stated
  with equal candor:
  - **Deletes and hard updates behind the cursor are not captured** — a
    row deleted upstream stays in the cell until a `--full-refresh`.
    Cursor-based incrementality is for append-heavy sources; CDC is a
    different feature and is not promised.
  - **Cursor monotonicity is an author assertion the engine cannot
    verify.** A backdated `updated_at`, a non-monotonic surrogate key, or
    ingestion clock skew beyond the lookback window is **silent data
    loss**: `greatest` faithfully protects the watermark while rows landing
    behind it are never read. `lookback` is the mitigation; choosing a
    truly monotonic cursor is the requirement.
  - **NULL cursor values** load once at bootstrap and never again (§1's
    bind-time warning names this).
  - **An update-timestamp cursor with an anti-join transform drops
    updates** (§2) — the docs contrast anti-join vs MERGE precisely here.
- **The work, honestly (dependency order):**
  1. Schema: `Incremental { cursor, lookback }` as an inner
     `deny_unknown_fields` struct on `Source::Connection` and
     `ResolvedSource::Connection`; duration-string parser; resolve-time
     validation (identifier shape, lookback parse); tests asserting the
     **error text** for malformed blocks, not just happy-path parse
     stability.
  2. Engine: watermark existence-check + typed read in `bind_source`
     (no pre-`BEGIN` DDL); the staged-delta bind path (quoted identifier,
     typed literal, TEMP TABLE + view); cursor type/existence/nullability
     checks with the §1 error texts; **`bind_source` signature change**
     returning per-source advances to `run`; in-transaction table creation
     + `greatest`-advance upsert with the explicit empty-delta skip (unit
     test pinning the NULL-`greatest` edge); spill configuration
     (`temp_directory`, memory limit); `--full-refresh` through `cli.rs`
     into `run` with the announce line; `--verify-replay` (post-commit
     replay in a rolled-back transaction + exact per-table multiset
     comparison); the pre/post-transform shrink warning; the `__datamk_%`
     prefix check in `verify`; the no-grain resolve-time warning; staged
     row counts in `run` output.
  3. `datamk status` watermark block and the `rollback` rewind printout
     (§4) — the read side that makes replay and rollback legible from a
     laptop with bucket credentials.
  4. Scaffold/docs: **live** anti-join transform SQL (not a comment) paired
     with any scaffolded `incremental:` block; the anti-join-vs-MERGE
     contrast; the `CREATE OR REPLACE` prohibition under incremental
     sources; README edge-cases section (deletes, monotonicity, NULLs,
     lookback, bootstrap/full-refresh cost, init-timeout interaction).
  5. e2e: two-execution kind/MinIO run asserting the second execution's
     staged delta excludes the first's rows, that rollback-then-run
     re-delivers them, that `--verify-replay` passes on the scaffolded
     transform and fails on a deliberately duplicating one, that a
     truncating transform trips the shrink warning, and that
     expire/cleanup behaves over a watermark-bearing lineage. Plus a
     credential-gated warehouse integration test — **new infrastructure,
     preceded by a research spike**: establish how to observe
     bytes/rows-scanned through the community extension (job stats,
     `INFORMATION_SCHEMA.JOBS`, or a controlled small-table experiment),
     then assert (a) the cursor predicate pushed (bytes scanned ∝ delta,
     not table) and (b) **no-gap timestamp round-trip** — a
     timezone-carrying cursor read by DuckDB and pushed back as a predicate
     selects exactly the rows after the mark, no dupes required, no gaps
     tolerated. This is the sharpest external assumption; prove it where it
     runs, per the ADR 0004 house rule.
- Nothing renders differently at deploy; no overlay or profile field is
  added. The feature is engine-level and works identically in local
  direct-attach mode (with the verify-after-commit asymmetry §1 documents).

## Alternatives considered

- **Engine-owned materialization (model kinds — the sqlmesh/dbt shape).**
  A `materialize:` block per transform (append/merge + key) over pure
  SELECTs, with the engine issuing DDL/DML. **Declined on principle, not
  deferred for maturity** — the fuller argument is in Context. The steelman
  is real: it would remove boilerplate, make replay-safety correct by
  construction, and tell the engine which tables a transform writes (which
  `verify` and future lineage would value). But owning the query is a
  one-way door with a built-in ceiling — every physical strategy the
  framework didn't anticipate becomes a fight with its query generator, and
  the abstraction subtracts exactly the capability (full SQL semantics)
  that is already well understood. The one concrete capability declined
  with it is **declarative partial backfill** ("re-run March only" as a
  first-class operation, with restatement of downstream models): datamk's
  only lever for history rewritten behind the cursor is `--full-refresh`,
  which is all-or-nothing. That is the honest price of the boundary, paid
  knowingly — partial backfill is precisely where interval accounting and
  restatement cascades make the framework shape explode in complexity.
  Replay-safety is instead verified behaviorally (§2), and the reopening
  condition is §2's, not an incident counter.
- **A `replay_safe: true` attestation on incremental sources.** Considered
  as a way to make the author's promise greppable and reviewable. Declined:
  with `--verify-replay` checking the behavior itself, an attestation is
  ceremony — a collected promise where a verified fact is available, which
  is the same trade declined at the query level.
- **Rejecting incremental cells with no declared grain.** Declined as too
  coarse (the engine cannot know which exports derive from the incremental
  source without parsing SQL, and grain is not always the natural contract
  for every export); the standing warning plus `--verify-replay` covers the
  gap without a false constraint.
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
  makes the accounting (`max` of what was *actually delivered*) exact. The
  costs staging accepts in exchange (loss of transform pushdown, full local
  materialization) are stated in §1 rather than hidden.
- **Templating/conditionals in transform SQL** (`is_incremental()` à la
  dbt). Rejected: datamk transforms are plain SQL files by design; the
  bootstrap case is already handled by the unfiltered first read, so the
  conditional buys nothing but a template language — the symptom, per
  Context, of a tool that inserted itself into the query.
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
