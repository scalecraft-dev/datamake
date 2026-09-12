# Incremental source loading

A `connection` source with an `incremental:` block is read only past a persisted
watermark (`cursor > watermark`) instead of in full on every run.

```yaml
sources:
  events:
    connection: crm
    table: analytics.events
    incremental:
      cursor: updated_at   # monotonic column: timestamp, date, or integer
      lookback: 2h         # optional; time cursors only
```

## `incremental:` keys and `run` flags

| Name | Type | Default | Meaning |
|---|---|---|---|
| `cursor` | bare column name (`[A-Za-z_][A-Za-z0-9_]*`) | required | Column whose max becomes the new watermark. Must be timestamp, date, or integer. |
| `lookback` | duration `<int><s\|m\|h\|d>`, e.g. `30m`, `2h`, `1d` | none | Reads `cursor > watermark − lookback` for late rows. Time cursors only; `0h` is an error. |
| `datamk run --full-refresh` | flag | off | Re-reads every incremental source unfiltered and rewrites each watermark to the fresh `max(cursor)`. No-op with a warning on a cell with no incremental sources. |
| `datamk run --verify-replay` | flag | off | Re-runs the transforms a second time against the identical staged delta (rolled back afterwards) and fails the run before publish if any output table's rows differ. Warehouse is not read again. |
| `datamk deploy --init-timeout <secs>` | int | 300 | How long deploy waits for the Init Job's `run`. A first deploy is a bootstrap full scan and may need more. |
| `DATAMK_MEMORY_LIMIT` | env, e.g. `2GB` | 75% of cgroup limit | Caps DuckDB memory so a large staged delta spills to `temp_directory` (always set) instead of OOM-killing the pod. |

Validation: `cursor` shape, unknown keys, missing `cursor`, and `lookback` parsing
fail at resolve time (offline). Cursor existence and type fail at bind time on
`datamk run`; `datamk verify` does not check them. A nullable cursor is a
bind-time warning.

## Delivery contract

- **At-least-once.** A run's view contains every row past the watermark plus,
  possibly, rows already seen (lookback, rollback-then-rerun, `--full-refresh`).
- **Bootstrap** (no watermark yet, or `incremental:` just added) is a full scan
  through the same view. The whole delta is staged as a local DuckDB temp table
  before any transform runs.
- **Watermark** lives in the engine-owned table `__datamk_watermarks` in the
  cell's DuckLake catalog, committed in the same snapshot as the rows. It only
  advances (`greatest(old, new)`). `rollback` rewinds it with the data; the next
  run re-ingests the discarded rows. Same table in direct-attach (local
  `catalog:`) mode.
- `__datamk_` is a reserved prefix: `verify` fails if a transform creates any
  other table matching `__datamk_%`. The table is invisible to `interface:`.

## Landing the delta: `materialize:`

Every file under `sql/` is a single bare `SELECT` with no trailing `;`. The
engine composes the DDL/DML around it. Table name is the file stem; two entries
with the same stem is a resolve-time error.

```yaml
transforms:
  - sql/stg_events.sql          # bare path = materialize: replace
  - sql: sql/fct_events.sql
    materialize: upsert         # append | upsert | replace
    key: [event_id]             # required for append/upsert; forbidden for replace
  - sql/daily_rollup.sql        # replace; reads fct_events, not events
```

| `materialize:` | `key:` | Composed DML | Use for |
|---|---|---|---|
| `replace` (default) | forbidden | `CREATE OR REPLACE TABLE … AS (<select>)` | Rollups, dims, anything rebuilt from scratch. |
| `upsert` | required, unique and non-NULL in the delta | `CREATE TABLE IF NOT EXISTS` + `MERGE` (last delivery wins) | Accumulators where updates matter. |
| `append` | required, unique and non-NULL in the delta | `CREATE TABLE IF NOT EXISTS` + anti-join insert (first write wins) | Immutable event logs. |

Hard errors, all before anything is written:

- A `replace` model whose SQL text contains an incremental source name as a whole
  token (resolve time; word-boundary scan, not a parser).
- Duplicate `key` values in the staged delta. Dedupe in the SELECT with `QUALIFY row_number() OVER (…) = 1`.
- NULL in a `key` column.
- Schema drift (new, dropped, or retyped column) for `upsert`/`append`. Recover
  with `--full-refresh` or `datamk attach`. `replace` recreates at the new shape.
- A file that is not one bare SELECT (hand-written DDL, trailing `;`) fails at
  subquery wrap.

Other rules:

- `--full-refresh` on `upsert`/`append` rebuilds the table from a full re-read.
  It changes nothing for `replace`. There is no ALTER path inside the pipeline.
- An export sourced from an `upsert`/`append` table inherits `key:` as its
  `grain:`; an explicit `grain:` must contain every key column. A `replace` table
  inherits nothing.
- The composed statements are written to `.cell/materialize/<table>.sql` every
  run (audit and portability; overwritten; not a valid `transforms:` target).
  `DATAMK_MATERIALIZE_DIR` redirects them; the container image sets it to
  `/tmp/materialize` because the cell mounts read-only there.
- One-off backfills and manual corrections: `datamk attach -f cell.yaml -p prod`
  and run SQL against the lake directly.

## Observability

- `run` logs one line per source: `staged delta past watermark source=… staged_rows=… watermark=…` or `staged full table (bootstrap)`.
- `run` warns when any existing table shrank during a run with incremental sources (likely a `CREATE OR REPLACE` over a delta).
- `verify` warns on every run when an incremental source exists and no export declares a `grain:`.
- `status` prints a watermark block per source: cursor, current mark, last delta size, or `absent — next run bootstraps a full scan`.
- `rollback` prints the watermark rewind and the rows the next run will re-ingest.

## Gotchas

- Deletes and updates behind the cursor are never captured. Reconcile with a periodic `--full-refresh`.
- Cursor monotonicity is unverified. Rows landing behind the watermark beyond `lookback` are lost silently.
- NULL cursor values are staged once at bootstrap and never again.
- `cursor: updated_at` with `materialize: append` drops updates. Use `upsert`.
- `upsert` has no ordering column: a re-delivered staler row overwrites the stored one.
- The replace-model source scan false-positives on a source name in a `--` comment and misses names hidden behind CTE aliases.
- `--verify-replay` cannot detect truncation (a replace over a delta is idempotent) and fails on non-deterministic SQL (`now()`, `random()`).
- Direct-attach mode: a `verify` failure leaves the bad snapshot and advanced watermark committed. Fix the transform, then `--full-refresh`.
- `--full-refresh` is a `run` flag, never deploy config. Run it as a one-off job (`kubectl create job --from=cronjob/… -- run --full-refresh`).
- A BigQuery view source without `incremental:` materializes in full every run; with it, bytes returned scale with the delta but bytes billed depend on the view's SQL.
- Cursor-predicate pushdown has no bytes-scanned integration test yet. Measure before relying on the cost model for a connector.

Design rationale: [ADR 0005](../adr/0005-incremental-source-loading.md), [ADR 0008](../adr/0008-declarative-incremental-materialization.md).
