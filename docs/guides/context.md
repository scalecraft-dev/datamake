# The context document

Every cell serves a **context document**: its interface as JSON, with column
meanings, the exact query grammar, measurements from the real rows, and build
provenance. It is a projection of `cell.yaml` and the build; nothing separate
to maintain.

## Fetch it

```bash
# Served (same auth as the data routes)
curl -H "Authorization: Bearer $TOKEN" https://orders.data.internal/context
curl -H "Authorization: Bearer $TOKEN" "https://orders.data.internal/context?include=docs"
curl -H "Authorization: Bearer $TOKEN" "https://orders.data.internal/context?include=check"
curl -H "Authorization: Bearer $TOKEN" "https://orders.data.internal/context?view=index"
curl -H "Authorization: Bearer $TOKEN" "https://orders.data.internal/context/orders_daily@2"
curl -H "Authorization: Bearer $TOKEN" "https://orders.data.internal/context?terms=net_revenue,nr"
curl -H "Authorization: Bearer $TOKEN" "https://orders.data.internal/context?model=invoice"

# Portable (no server, no token). Inlines docs and the census by default.
datamk context -f cell.yaml [-p prod] [--out context.json] [--no-docs] \
                [--export orders_daily@2] [--terms net_revenue,nr] [--model invoice] \
                [--view index]
```

Query params on `/context` are a closed set: `include` (`docs`, `check`),
`terms`, `model`, and `view` (`full` default, `index`). `/context/<route>`
accepts `include` and `terms` only — `model` and `view=index` are both 400
there (`model` is a whole-cell view; a single-export door is already the
index's whole point). Anything else is 400. An unknown `<route>` is 404
naming the routes that exist; an unknown `model` is 404 naming the models
that exist.

`?view=index` projects every `exports[]` entry to identity, claims, and
affordances — no `schema`, `check`, `probe`, or `semantic[]` bodies;
declared column names ride `columns[]` and bound Ossie datasets ride
`semantic_datasets[]` instead. `?model=` and `?terms=` (without a route)
apply the same projection automatically, since this door never narrows by
route — asking for one model or a term subset has no reason to also pay for
every export's full body. `/context/<route>?terms=` is exempt: that door's
whole point is the route's full export. Measured on a real 42-export/
47-dataset cell: default `/context` is 429 KB, of which `exports[]` is
383 KB; `?view=index` and `?model=` both land under 10% of the full
document's bytes on an equivalent synthetic cell.

Every data-route response (200 and 404) carries:

| Header | Value |
|---|---|
| `Link` | `</context>; rel="describedby"` |
| `X-Datamk-Context-Digest` | Interface digest. Moves on interface changes, not data refreshes. |
| `X-Datamk-Execution` | Served execution number (published profiles only). |

## What's inside

Flat document. A record with `from` is a **claim** (origin per field:
`cell.yaml`, `warehouse`, or a modeling tool); a block with a timestamp is a
**measurement**. Absent facts are omitted or `null`, never zero.

| Field | Meaning |
|---|---|
| `datamk_context` | Document format version. |
| `status` | `draft` \| `verified_at_source` (live check passed) \| `verified` (published, verify-gated build). |
| `grain_verified` | Whether grain uniqueness was checked. |
| `description`, `from` | Cell prose and its origin. |
| `exports[]` | Per export: `name`, `version`, `route`, `contract`, `description`, `grain`, `freshness`, `schema`, and exactly one of `query` / `binding`. Under `?view=index`/`?model=`/`?terms=` (no route): projected — see below. |
| `exports[].schema.<col>` | `type`, optional `unit`, `description`, `from`. Empty (`{}`) under the index projection. |
| `exports[].columns[]` | Index projection only: declared column names, in order — no type/unit/description/`from`. Empty (omitted) on the full document. |
| `exports[].query` | `filters`, `filter_semantics`, `limit_default` 100, `limit_max` 1000, `offset_max` 1000000, `sample_request`. |
| `exports[].probe` | At swap: `at`, `rows`, `coverage` (min/max per grain col), `values` (per col, with `complete`), `null_rows` (per grain col), `example_request` (from one real row). Absent under the index projection. |
| `exports[].check` | Live-check measurement (bound exports): `at`, `check` (`grain_unique`, or `schema` when no grain is declared), `grain`, `rows`, `distinct_grain`, `null_rows`, and `columns` under `?include=check`. Absent under the index projection. |
| `exports[].check.columns` | The column census, bound exports only, `?include=check` (served) or the portable `datamk context` emission (always) — see "The census on the wire" under `bind:` below. |
| `exports[].freshness` | Author's `freshness:` verbatim. Advisory only, never compared against anything. |
| `exports[].freshness_observed` | Discovered exports with a `freshness` claim and a live check: `at`, `intervals_end` (`deployed.intervals.end` as last synced), `age_seconds`. A number beside the claim, not a verdict on it. |
| `upstreams[]` | `ref`, pinned `version` (usually `null`), attached `execution`, `data_as_of`. Last two absent for direct-attach upstreams. |
| `build` | `execution`, `snapshot_id`, `verify_outcome`, `finished_at`, `data_as_of`. Absent on bound-only cells. |
| `source_check` | `outcome`, `checked_at`, `datamk_version`, optional `data_as_of`. |
| `freshness` (top level) | Server poll telemetry, not data age. |
| `docs[]` | Per page: `target`, `source_path`, `media_type`; `sha256`, `bytes` after a release; `content` only under `?include=docs`. |
| `included` | Section names inlined — any subset of `["docs", "check"]`, `[]` on the default served variant, both on the default portable emission. Absent means the server predates this field. |
| `include_request` | Relative URL to fetch the docs variant. |
| `index_request` | Relative URL to fetch the index projection (`context?view=index`). |
| `definitions[]`, `missing_terms` | Glossary (always present) and unmatched `terms=` tokens. |
| `semantic_models[]` | Apache Ossie index (always present, `[]` without a bound `semantic_model:`): `name`, `description`, `file`, `datasets`, `bound`, `metrics` counts. |
| `semantic` | Ossie provenance, a measurement outside every digest: `synced_at`, `content_sha256`, `resolved`, `files`; `checked_at` once `datamk verify` has run. Present iff `semantic_model:` is declared and fresh. |
| `semantic_model` | One semantic model in full, `?model=`/`--model` only: `name`, `description`, `ai_context`, `file`, `datasets[]` (unbound ones carry `routes: []`), `relationships[]`, `metrics[]`. |
| `semantic_matches[]` | `?terms=`/`--terms` hits against an Ossie dataset, field, metric, or synonym: `token`, `kind`, `model`, `dataset`, `field`, `description`. Every hit returned — a token colliding across two models lists both. |
| `exports[].semantic[]` | Every Ossie dataset bound to this route: fields (with `verified`/`reason` once checked), `primary_key`/`primary_key_check`, relationships touching it, metrics fully checkable from this route alone plus `metric_refs` pointers to the rest. Absent under the index projection. |
| `exports[].semantic_datasets[]` | Index projection only: `model/dataset` names bound to this route, in place of `semantic[]`'s full bodies. Empty (omitted) on the full document. |
| `data` | `served_here`, `channels`. |
| `notes` | Engine notes, e.g. why status is `draft`, or that a bound semantic model is stale. |

`sample_request`, `example_request`, `include_request`, `index_request`,
and `definitions_request` are relative to the document's own URL (RFC
3986), so mounts in a multi-cell server work.

## Author the meaning

```yaml
cell: orders
description: Daily order revenue by region.        # one line
docs: docs/overview.md                             # optional, one path

interface:
  - name: orders_daily
    version: 2.1.0
    description: One row per (order_date, region) with the summed order revenue.
    docs: docs/orders_daily.md                     # optional, one path
    grain: [order_date, region]
    freshness: daily                               # advisory
    schema:
      order_date: date                             # bare type still works
      revenue: { type: decimal, unit: USD, description: Gross revenue, before refunds. }

definitions:                                       # or: definitions: definitions.yaml
  - term: net_revenue                              # ^[a-z0-9][a-z0-9_.-]*$, ≤64 chars
    aliases: [nr, revenue_net]                     # ≤5, same grammar
    description: Invoiced revenue less credit memos.
    docs: docs/terms/net_revenue.md                # optional
    applies_to: [flight_spend@1.invoice_amount, margins@2]   # route.column or route; omit = cell-wide
```

| Key | Rule |
|---|---|
| `description` | Cell: one line. Export and column: ~2 sentences. Length-capped at parse. |
| `unit` | Token, ≤16 chars, no whitespace. |
| `docs` | One relative path per level. ≤64 KiB per page, ≤256 KiB per cell. Empty, non-UTF-8, or oversized is a parse error. No `/docs/:name` route. |
| `definitions` | Inline list or one file path. Lookup by term or alias, case-insensitive, exact only. |
| `terms=` | Narrows `definitions[]` and `docs[]` to those terms' pages. Served: unknown term is 200 with `missing_terms`. `datamk context --terms`: unknown term exits non-zero, naming the known-term count and up to 8 nearest matches (prefix, then substring, then same-first-3-characters) — never the whole vocabulary. `--model` naming an unknown model gets the same treatment. |

## Semantic models (Apache Ossie)

`semantic_model:` ingests business meaning — field semantics, synonyms,
metric definitions, join relationships — from
[Apache Ossie](https://github.com/apache/ossie) documents authored outside
datamake (a `dir:` or a `git:` source). datamake never authors Ossie, never
evaluates a metric, never synthesizes a join; it snapshots (`datamk sync`),
plan-checks every claim it can against the built tables (`datamk verify`,
`DESCRIBE`, never a row read), and serves the result through four tiers —
see [semantic-model.md](semantic-model.md) for authoring, sync, and verify.

| Door | Returns |
|---|---|
| `/context` | `semantic_models[]` — name, description, file, dataset/bound/metric counts. |
| `/context/<route>` | `exports[].semantic[]` — every dataset bound to the route, in full. |
| `/context?model=<name>` | `semantic_model` — one model in full, composable with `terms`; `exports[]` projected to the index shape. |
| `/context?terms=<t>` | `semantic_matches[]` — Ossie dataset/field/metric names and synonyms join the `definitions:` lookup; `exports[]` projected to the index shape. |
| `/context?view=index` | Every `exports[]` entry projected: identity, claims, `columns[]`, `semantic_datasets[]` — no `semantic[]` bodies. |

`datamk mcp` adds a resource per model, `datamk://<mount>/semantic/<model>`
(the same document `?model=` returns), plus `datamk://<mount>/context/index`
(the same document `?view=index` returns).

## Meaning without rows: `--no-data`

```bash
datamk serve -f cell.yaml --no-data
```

| Effect | |
|---|---|
| Data routes | Not mounted (404). |
| `/context` | Full meaning, aggregate measurements, docs pages. `values` and `example_request` withheld. |
| `channels` | Set in the profile (`channels: ["warehouse: analytics.orders_daily"]`); surfaces under `data.channels`. |

## A contract with no rows to copy: `bind:`

```yaml
sources:
  pii: { connection: crm, table: raw.customers }
interface:
  - name: customer_pii
    version: 1.0.0
    bind: pii                       # no transform, no snapshot
    schema: { id: bigint, email: string }
```

| Rule | |
|---|---|
| Bindable | A raw file source or a connection source with `table:`. Not `query:`. |
| Document | `query: null` plus `binding: {source, object, connection}`, verbatim from `cell.yaml`, never profile-resolved. |
| `datamk run` | Never computes a bound export. An all-bound cell has no snapshot; `run` refuses, use `verify` and `context`. |
| `datamk verify -p prod` | Live-checks schema and grain against the warehouse (native types where available, BigQuery today) and takes the column census (`check.columns`). Writes `.cell/source_check.json` with a `cell.yaml` digest. |
| The census on the wire | `check.columns` (per declared column: `null_rows`; for a non-grain column of at most 50 distinct values, `distinct` and up to five `top_values`; otherwise `distinct_over_50: true`, `top_values` withheld under `--no-data`) is served only under `?include=check` — omitted by default, including under `?view=index`. The rollup (`check`, `grain`, `rows`, `distinct_grain`, `null_rows`, `at`) is on every record regardless. The portable `datamk context` artifact always inlines it when present — no `--no-check` flag; a file cannot be re-requested. |
| Prose vs data | A column `description` or a definition with `applies_to: [route.column]` that says the column is empty (`currently null`, `backfill pending`, `not yet populated`, `not populated`, `always null`, `empty`) while the census measured rows populated draws a warning from `verify` and `release` and a note in `notes[]`. Never a failure. |
| `datamk context` | Embeds `source_check` only while that digest matches the current `cell.yaml`. |
| Status | Passing live check gives `verified_at_source`, never `verified`. |
| Column prose | Warehouse column descriptions fill undescribed bound columns with `from.description: "warehouse"`. Authored prose wins. Not applied to materialized exports. |
| `datamk deploy` | Allowed unless the target has nothing to run. |

## Mesh manifest

```bash
datamk mesh emit --cells cells.yaml --out mesh.json
datamk mesh emit --store s3://acme/cells --url-template "https://{name}.data.internal" --out mesh.json
```

`cells.yaml` entries carry `name`, `url`, optional `auth_hint` (a credential
name, never a token). The emitter fetches each `/context` and copies
description, exports, and `context_digest`. Static file; never served by
`serve`.

## Gotchas

- `verify` fails on a `description` for a column the source no longer has, and on `contract: supported` without a non-empty export `description` (a `docs:` page does not count).
- On a bound export, `verify` scans the object once per low-cardinality column for the census, on top of the grain check's full scan. Id-like columns cost one shared aggregate. Run it on a schedule, not per request.
- `datamk release` digests `cell.yaml` prose and docs content; a change without a version bump warns. Warehouse prose is never in that digest.
- Interface digest = `/context` `ETag` = `/openapi.json` `info.version` = manifest `context_digest`. Prose and docs-content edits do not move it. Adding, removing, or renaming a `docs:` page does. `?include=docs` has its own `ETag` (`"<digest>~docs.<hash>"`); `?include=check` adds `~check`; `?view=index` adds `~index`; `If-None-Match` gives 304 for the exact variant it names.
- Docs edits move `content_hash`, so a deploy rolls the workload.
- The Kubernetes ConfigMap carrying cell content is capped at 1 MiB, shared with `cell.yaml` and transform SQL.
- No caller-supplied SQL, filters, or projections on any route. Use `datamk attach` for SQL.
- `datamk mcp` serves this same document over stdio. See [mcp.md](mcp.md).

Design rationale: [ADR 0012](../adr/0012-cell-context-document.md),
[ADR 0013](../adr/0013-long-form-docs-pages.md),
[ADR 0015](../adr/0015-flat-context-document.md),
[ADR 0017](../adr/0017-definitions.md),
[ADR 0018](../adr/0018-ossie-ingest.md).
