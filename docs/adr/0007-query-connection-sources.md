# ADR 0007 — First-class `query:` connection sources

- **Status:** Proposed
- **Date:** 2026-07-13
- **Deciders:** Datamake team
- **Author:** @scottypate
- **Depends on:** ADR 0003 (connection sources — reopens its deferred
  `query:` decision under its own named trigger), ADR 0006 (jobs-API
  routing and once-only staging, reused verbatim)

## Scope

A `connection` source may declare a **`query:`** — warehouse-dialect SQL
executed server-side — instead of a `table:`. The result stages through
ADR 0006's jobs-API path unchanged. This ADR covers the contract shape,
the routing, the validation story (including what deliberately cannot be
validated), the v1 refusal of `incremental:` composition, and the honest
costs: a scoped narrowing of the same-cell-everywhere guarantee and a
reduction in `verify`'s coverage that demands a compensating test.

## Context

ADR 0003 deferred `query:` sources with a named reopening condition:
"if scan-with-pushdown proves insufficient for a real table." That
condition has now been met in production, twice over, by one source:

- `summarydata.campaign_group_spend_by_minute` — 2.57B rows of
  minute-grain spend behind a silver view. As a view it routes through
  the jobs API (ADR 0006), where its full result exceeds BigQuery's
  ~10GB anonymous-result ceiling and cannot be read in one pass at all.
- The consuming cell (flight-spend) never uses minute grain. Its
  transforms collapse the source immediately; the finest grain consumed
  anywhere is `(advertiser, parent_campaign_group, campaign_group,
  hour)`. A server-side `SUM … GROUP BY` to hour grain is **lossless
  for the cell** and shrinks the result ~60×, comfortably under the
  ceiling — and shrinks what crosses the wire and lands in the Builder
  by the same factor, every run.

Scan-with-pushdown cannot express that aggregation; no predicate can.
The alternatives were traced and rejected for this shape: bulk transport
(ADR 0006 §3a) would move 2.57B rows hourly to compute a monthly sum —
viable-at-all, insane as an endpoint; a bounded-history
`incremental.start:` is disqualified outright for this cell because its
prior-spend logic scans ~550 days back — clipping history silently
under-counts prior spend and **over-invoices**, the worst silent-wrong-
number failure a finance cell can have.

ADR 0005's boundary — *abstract the state, never the query* — is not
crossed here: the engine still never parses, wraps, or rewrites SQL.
`query:` is more author-owned SQL, in the author's own contract file, at
the same trust tier as transforms. What changes is *where* some shaping
runs: in the warehouse, before the rows ever leave it.

## Decision

### 1. Contract shape

```yaml
sources:
  raw_spend_hourly:
    connection: dw_silver
    query: |
      SELECT advertiser_id, parent_campaign_group_id, campaign_group_id,
             hour, CAST(SUM(total_spend) AS NUMERIC) AS total_spend,
             CAST(SUM(media_cost) AS NUMERIC) AS media_cost
      FROM `${connection.project}.summarydata.campaign_group_spend_by_minute`
      GROUP BY 1, 2, 3, 4
```

- `query:` and `table:` are **exactly-one-of**, validated at parse time
  with an error naming both fields. All other source fields keep their
  meaning.
- It lives in `cell.yaml`, never the profile: what rows the cell
  consumes is business logic — the same category as a transform. The
  profile connection is unchanged.
- **The contract/environment seam is a mechanism, not a convention:
  `${connection.project}`.** The engine substitutes the source's
  resolved connection `project` for the reserved `${connection.project}`
  binding at resolve time (the `connection.` prefix is engine-owned;
  env-var expansion cannot collide with it since env names cannot
  contain `.`). This is the `query:` analog of what `qualify()` does
  for `table:` — the engine owns qualification of the reference. The
  project value never appears in the contract; dev/prod project splits
  work because a different profile resolves a different project.
  *Why a mechanism is required, learned live:* unqualified
  `dataset.table` in a BigQuery job resolves against the project the
  job runs in — which under split billing is `billing_project`, not
  `project` — so a bare reference fails at bind (`Not found: Dataset
  <billing>:<dataset>`) on any cost-allocated connection. An author who
  forgets the placeholder gets exactly that loud bind-time error, never
  a silent misread; an author who hardcodes a project still leaks
  environment undetected, which review must catch.

### 2. Routing and staging

A `query:` source is jobs-routed **by construction**: it skips
`classify_objects` (there is no table name to classify), skips
`qualify()`, and stages exactly once via ADR 0006 §3 —
`CREATE TEMP TABLE __jobs_<idx> AS SELECT * FROM
bigquery_query('<project>', '<author query>', billing_project := …)` —
with the same narration, row-count telemetry, and pre-`BEGIN` placement.
The connector's only transformation of the author's string is `esc()`
for delivery; no identifier rewriting, no predicate injection.

If a `query:` result itself exceeds the response ceiling, ADR 0006 §3a's
escape hatch applies unchanged (the export statement wraps the author's
query instead of a generated `SELECT *`).

### 3. `incremental:` on a `query:` source is refused in v1

The shipped watermark mechanics append `WHERE cursor > <literal>` to an
engine-generated `SELECT *`. Appended after an author's `GROUP BY` it is
a syntax error; wrapped around the query it filters *post-aggregation* —
and pre- vs post-aggregation filtering is a real semantic fork that
should be designed deliberately, not resolved implicitly. v1 therefore
rejects the combination at resolve time:

```
source 'raw_spend_hourly': `incremental:` is not yet supported on a
`query:` source. Use `table:` with `incremental:`, or drop `incremental:`
— a `query:` source is re-read in full each run.
```

Composition (likely: the engine wraps the author query as a subselect
and filters on a projected cursor column, post-aggregation semantics
stated explicitly) is the first follow-up, taken up when a real cell
needs it.

### 4. Validation, stated as a split

- **Offline (`verify`, resolve):** exactly-one-of `table:`/`query:` and
  the `${connection.project}` substitution; nothing else. The engine
  cannot validate the query's well-formedness, output columns, or grain
  offline — offline validation of a `query:` source is **strictly
  weaker** than `table:`'s shape check, and the docs say so.
- **Bind time, before the real read: a dry-run preflight.** The engine
  dry-runs the author's query (`dry_run := true` — free, unbilled,
  no scan). A malformed query fails loud pre-`BEGIN`, earlier and
  cheaper than the first staging read; a successful dry run narrates
  the query's exact `total_bytes_processed` — the per-run scan cost,
  made exact where ADRs 0006/0007 previously had only prose caveats
  (this delivers ADR 0006 §6's narration promise for the jobs path).
  Narration-only, never gating: a dry-run *transport* failure warns and
  proceeds to the real read, which fails loud on its own if genuinely
  broken. Bytes are the narrated primitive; any dollar figure is
  labeled an on-demand estimate.
- **Staged types are narrated.** After staging, the engine DESCRIBEs
  the temp table and logs the staged column types. This is the honest
  ceiling of engine help for the known type hazard below: a column the
  author expected numeric showing up as `VARCHAR` becomes visible at
  the source boundary instead of as `sum(VARCHAR)` three transforms
  deep.
- **Known hazard, bind-undetectable: BigQuery numerics beyond DuckDB's
  range degrade silently to VARCHAR.** `BIGNUMERIC` (and full-range
  `NUMERIC` aggregates, e.g. `SUM(BIGNUMERIC)`) exceed DuckDB
  `DECIMAL(38,·)`; the extension maps such result columns to VARCHAR
  with no warning. There is no extension-exposed result schema for an
  arbitrary query (dry runs return bytes, not schema; a `LIMIT 0` read
  yields only DuckDB-side types, which cannot distinguish degraded from
  legitimately-string columns) — so the engine cannot detect this
  authoritatively and does not pretend to. The author must
  `CAST(… AS NUMERIC)` (or another representable type) in the query
  body. The CAST is an **author correctness assertion** — it can
  overflow or round if values exceed the target — and is therefore
  covered by §5's gated test, not by any engine check. The engine never
  auto-casts: choosing a precision-losing target scale is a value
  decision it must not make silently.
- **Output interface:** `verify` on the built snapshot is unchanged.

### 5. The compensating test is the price of the feature

`verify` checks the declared interface against the built output;
`--verify-replay` re-runs transforms against the staged local copy. A
server-side aggregation happens **upstream of both** — a wrong `GROUP
BY` or a filter typo produces wrong-but-schema-valid numbers that pass
every check datamake has and never enter the replay. For a finance cell
that is the defining risk of this feature, and no engine mechanism can
cover it without parsing SQL (declined, ADR 0005).

Therefore: **a `query:` source that shapes values requires a gated
warehouse correctness test** asserting the shaped read equals the
locally-computed aggregate over a known fixture — and, for flight-spend
specifically, pinning the "hour is the finest consumed grain" claim so a
future transform that starts consuming minute grain turns the lossless
aggregation assumption into a loud failure instead of silent
under-reporting. This test is part of adopting the feature, not
optional hardening.

The convention (following ADR 0003's "gated behind credentials, skipped
when absent"): a `tests/` integration crate
(`tests/bigquery_query_correctness.rs`), runtime-skipped — early-return
when `DATAMK_TEST_BQ_PROJECT` is unset, so `cargo test` stays green
without credentials and CI-with-credentials runs it. Fixture seeding
needs BigQuery *write* to a test dataset, unlike every other integration
test, so seeding gates on a second variable
(`DATAMK_TEST_BQ_RW_DATASET`), documented as this test's unique
requirement. The test also covers the §4 CAST assertion (values
round-trip the cast without overflow for the fixture's ranges).

### 6. What does not change

- `table:` sources, their validation, storage-path routing and pushdown.
- The profile schema, `serve`, the interface contract, snapshot
  pinning, watermark bookkeeping.
- The injection posture: `query:` is author SQL in the author's own
  reviewed contract file — the same trust tier as transforms; end-user
  input never reaches it. `serve`'s query-building seam is untouched.

## Consequences

- flight-spend's oversized source becomes a small hour-grain read; the
  cell can go green with no transport mechanism and no history clipping.
- **Portability narrows, scoped and by construction.** A cell with a
  `query:` source runs only against connections of that warehouse
  family — GoogleSQL in the contract is not dialect-portable. Cells
  that stay on `table:` keep the full same-cell-everywhere guarantee.
  This is a permanent, documented trade the author makes per source.
- **Bytes-billed are unchanged in kind.** `query:` shrinks bytes
  *returned*; the aggregation still scans what the view's SQL scans,
  every run (ADR 0006's scan-cost caveat inherited verbatim). For
  flight-spend's 550-day lookback, expect full-history scan cost per
  run regardless.
- `verify`'s coverage boundary moves and the gap is owned: §5's gated
  correctness test is the stated compensation.
- The `init` scaffold and README present `query:` with the seam rule
  (never hardcode a project), the portability trade, and the §5 test
  obligation.

## Alternatives considered

- **Transport for the oversized result instead** (ADR 0006 §3a as the
  go-green path). Rejected as the endpoint for this cell: hourly bulk
  transfer of 2.57B rows to compute a monthly aggregate is pure waste,
  and the local materialization pressure lands on the Builder. §3a
  remains the floor for shapes `query:` cannot yet serve (an
  incremental view source's oversized bootstrap, §3).
- **`incremental.start:` bounded bootstrap.** Rejected for this cell as
  a correctness bug (over-invoicing via clipped prior-spend history)
  and deferred generally — it amputates data semantics rather than
  shaping a read, and belongs to a future ADR 0005 amendment if a real
  cell wants bounded history explicitly.
- **A `filter:` predicate field.** Already rejected in ADR 0006 — a
  predicate cannot express aggregation; this ADR is the full reopening
  it pointed at.
- **Engine-parsed/validated query shapes** (allow only
  SELECT-with-GROUP-BY templates, checkable grain). Rejected: the
  engine does not parse SQL (ADR 0005's boundary), and a template
  language over queries is the exact capability-subtracting abstraction
  that boundary exists to prevent.
