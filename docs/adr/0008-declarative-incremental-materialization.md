# ADR 0008 — Transforms are SELECT-only models; the engine owns materialization

- **Status:** Accepted (2026-07-14; revised in place pre-release — this document states the final design, not its drafting history). Supersedes ADR 0005's file-boundary reading of "abstract the state, never the query."
- **Date:** 2026-07-13
- **Deciders:** Datamake team
- **Author:** @scottypate
- **Depends on:** ADR 0004 (executions, the one-transaction build), ADR 0005 (watermark/state layer — retained verbatim), ADR 0007 (`query:` naming)

## Decision

There is one language for transforms.

1. **Every transform is a SELECT-only file.** No transform contains DDL or DML — the engine composes all CREATE/MERGE/INSERT. There is no raw-DML entry, no hooks block, no second language.

2. **Table = file stem.** `sql/fct_flights.sql` builds table `fct_flights`. One file, one table, no override; two entries resolving the same stem is a resolve-time error. Rename the file if you want a different name.

3. **`materialize:` is the strategy — the closed set of relationships a table can have to its prior state:**

   ```yaml
   transforms:
     - sql/stg_flights.sql          # bare path = replace: rebuild each run (the default)
     - sql: sql/fct_flights.sql     # accumulate: new delivery replaces stored row by key
       materialize: upsert
       key: [flight_id]
     - sql/spend_daily.sql          # rollup over fct_flights; rebuilt each run
   ```

   - **`replace`** (default): `CREATE OR REPLACE TABLE t AS (<select>)`. No key. Derived tables, rollups, dims.
   - **`upsert`**: MERGE by `key:` — a re-delivered key replaces the stored row. Requires a key-unique SELECT (write the QUALIFY in your SELECT; dedup is query logic and stays yours).
   - **`append`**: anti-join by `key:` — a re-delivered key is a no-op. Immutable events.

   `{rebuild, merge, add}` is complete — there is no fourth relationship. The set is closed: a new strategy ships only if it is a fixed DML template over (table, key, SELECT) with zero query semantics AND replay-safe either intrinsically (reconciles against existing state, like upsert/append) or structurally (safe in every context it is permitted, like replace under guard 4c). `unique_by`, SCD2, time intervals, partition directives all fail this and do not ship.

4. **Engine-owned guards, all before any DML, all hard errors:**
   - (a) **NULL in a key column** — under IN/equality semantics a NULL-keyed row can never be reconciled and accumulates forever.
   - (b) **Duplicate keys in the staged delta** (`GROUP BY key HAVING count(*) > 1`) — the engine owns this loudness; the database's MERGE demonstrably does not error on it (silently updates from an arbitrary duplicate, or double-inserts an unmatched key).
   - (c) **A `replace` model that references an incremental source's name** (word-boundary token scan of the model's SQL text against the engine-owned incremental source names): rebuilding from a delta replaces history with the delta — the founding incident. Read the accumulated table instead, or use upsert/append. The scan is a token match against names the engine owns, not SQL parsing; a name in a comment false-positives (delete the comment); indirection that evades it falls to the shrink detector. If incremental `Cell`/`Raw` sources ever ship (ADR 0005 deferred), the scanned name set must extend to them.
   - (d) **Schema drift** — the SELECT's shape diverged from the accumulated table: error naming the column, recover with `--full-refresh`. No `on_schema_change` mode matrix.

5. **Grain:** an export sourced from a keyed table (upsert/append) inherits `grain:` from the key — declared once; an explicit grain there may only extend the key (adds filterable columns). Exports over `replace` tables and anything else declare grain explicitly; the runtime grain-uniqueness check is unchanged.

6. **`--full-refresh`** rebuilds every table from a full unfiltered re-read. It is also the schema-migration path and the hard-delete reconciliation path. There is no ALTER path inside the pipeline.

7. **The engine writes `.cell/materialize/<table>.sql` every run** — the exact executed statements (re-execution-safe: staging is `CREATE OR REPLACE TEMP TABLE`). This is the audit and portability surface: the composed SQL is plain DuckDB, runnable anywhere, so the abstraction is inspectable and the data layer never depends on datamk to be read.

8. **What is deliberately outside the pipeline:** one-off surgery (manual fixes, backfill corrections) happens through a database client against the lake — `datamk attach` prints the connection SQL — visible as an intervention, not disguised as a model. Conditional "newer wins" merge is not expressible (stale re-delivery can clobber a fresher row; re-delivery windows carry equal-or-newer data in normal operation — accepted). Declarative partial/interval backfill is declined; `--full-refresh` is the only, coarse, honest lever.

## Context

A user built incremental accumulators under ADR 0005's model (author hand-writes all DML; engine abstracts only extraction state) and was one plausible edit away from silently replacing 351k accumulated rows with a 45-row delta — the house-style `CREATE OR REPLACE` pointed at a delta view. The guards available were behavioral and after-the-fact: replay verification is structurally blind to truncation (a destructive rebuild is idempotent), and the shrink detector cannot gate without SQL parsing, so it warns — and a warning in an hourly cron log is where safety goes to be unread. Detection is partial, noisy, and late; construction is total. ADR 0005's own reopening condition ("evidence that behavioral verification cannot hold the invariant in principle") was met.

The principle, corrected: **"abstract the state, never the query" draws its line through the transform file, not around it.** The SELECT — which rows the delta yields — is the query and belongs to the author. The CREATE/DELETE/MERGE/INSERT wrapper is byte-identical regardless of what the SELECT computes: that is state-transition machinery, and forcing authors to hand-write it violated the principle 0005 named. The engine composes fixed DML templates around an opaque SELECT it never parses, reads, or rewrites.

Intermediate drafts of this ADR kept hand-written DML files as a coequal "escape hatch" alongside declarative entries. Field testing killed that: two file contracts in one list was itself the dominant confusion source (two naming regimes, grain/key duality, "which shape do I need here"), and every hard design call gravitated toward "write the DDL yourself" as its escape route, regenerating the seam each time it was patched. The ruling that settled it: a materialization strategy is nothing but the CREATE/MERGE/INSERT statement, and the only design question is who types it. The engine does. Always.

## Consequences

- The accumulator boilerplate is gone (~90%, the founding user's own estimate) and the truncation failure is **unwritable** in a transform file — there is no author DML to get wrong, and the one dangerous declarative combination (guard 4c) is a resolve-time error.
- One mental model: SELECT + strategy. Contract-to-file navigation is mechanical (stem naming); cross-entry collisions, key-vs-grain consistency, and the replace/delta hazard are all checked offline, before any warehouse read.
- ADR 0005's watermark/state layer is untouched; its behavioral guards (replay verification, shrink detector, grain-uniqueness backstop, reserved-prefix check) remain as tripwires against engine bugs rather than author mistakes.
- Costs, named: "newer wins" is inexpressible; `--full-refresh` on a large accumulator is a full re-read and full bill; three strategies is the permanent set and requests for a fourth get the admission criterion, not a roadmap slot. Renaming a file renames its table — the old table persists in the catalog and a stale export `source:` serves stale data; follow-on guard (warn when a served export's table wasn't written by the current execution) is deferred.
- Migration (pre-release): a hand-written transform converts by keeping its SELECT and declaring a strategy.

## Verification record (condensed)

- **MERGE vs DuckLake** (bundled DuckDB 1.5.4): proven in/out of transaction, over TEMP VIEW deltas, across re-attach, idempotent under re-delivery, and through the rollback/verify-replay machinery. Duplicate-key MERGE was *disproven* as an error path (silent arbitrary-winner update / double insert) — which is why guard 4b is engine-owned.
- **Bootstrap introspection** (`CREATE TABLE … AS <select> LIMIT 0`): proven to short-circuit aggregation/windows (285µs vs 2.73s full run); fixed ~6–8ms DuckLake catalog overhead, one-time; canary test guards against planner regressions.
- **Eject/portability artifact**: proven bit-identical against an independent control cell across a 4-delta sequence, both directions, mid-lifecycle, with the artifact re-execution-safe in one connection. First attempt failed on recipe ergonomics (unlogged DROP, guards silently shed) — fixed via `CREATE OR REPLACE TEMP TABLE` staging and the written artifact with its header stating exactly what it does and does not carry.
