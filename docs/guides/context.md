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
curl -H "Authorization: Bearer $TOKEN" "https://orders.data.internal/context/orders_daily@2"
curl -H "Authorization: Bearer $TOKEN" "https://orders.data.internal/context?terms=net_revenue,nr"

# Portable (no server, no token). Inlines docs by default.
datamk context -f cell.yaml [-p prod] [--out context.json] [--no-docs] \
                [--export orders_daily@2] [--terms net_revenue,nr]
```

Query params on `/context` and `/context/<route>` are a closed set: `include`
(`docs` only) and `terms`. Anything else is 400. An unknown `<route>` is 404
naming the routes that exist.

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
| `exports[]` | Per export: `name`, `version`, `route`, `contract`, `description`, `grain`, `freshness`, `schema`, and exactly one of `query` / `binding`. |
| `exports[].schema.<col>` | `type`, optional `unit`, `description`, `from`. |
| `exports[].query` | `filters`, `filter_semantics`, `limit_default` 100, `limit_max` 1000, `offset_max` 1000000, `sample_request`. |
| `exports[].relationships[]` | Discovered exports: the tool's declared join keys, resolved in-cell. `column`, `to` (route whose grain is exactly `to_column`, or `null`), `to_column`, `to_one_verified` (target's last `check`: `true`/`false`/`null`). Absent when none declared. |
| `exports[].probe` | At swap: `at`, `rows`, `coverage` (min/max per grain col), `values` (per col, with `complete`), `null_rows` (per grain col), `example_request` (from one real row). |
| `exports[].check` | Live-check measurement (bound exports): `at`, `check`, `grain`, `rows`, `distinct_grain`, `null_rows`. Absent when no grain is declared. |
| `exports[].freshness` | Author's `freshness:` verbatim. Advisory only, never measured. |
| `upstreams[]` | `ref`, pinned `version` (usually `null`), attached `execution`, `data_as_of`. Last two absent for direct-attach upstreams. |
| `build` | `execution`, `snapshot_id`, `verify_outcome`, `finished_at`, `data_as_of`. Absent on bound-only cells. |
| `source_check` | `outcome`, `checked_at`, `datamk_version`, optional `data_as_of`. |
| `freshness` (top level) | Server poll telemetry, not data age. |
| `docs[]` | Per page: `target`, `source_path`, `media_type`; `sha256`, `bytes` after a release; `content` only under `?include=docs`. |
| `included` | Section names inlined (`[]` or `["docs"]`). Absent means the server predates docs pages. |
| `include_request` | Relative URL to fetch the docs variant. |
| `definitions[]`, `missing_terms` | Glossary (always present) and unmatched `terms=` tokens. |
| `data` | `served_here`, `channels`. |
| `notes` | Engine notes, e.g. why status is `draft`. |

`sample_request`, `example_request`, and `include_request` are relative to
the document's own URL (RFC 3986), so mounts in a multi-cell server work.

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
| `terms=` | Narrows `definitions[]` and `docs[]` to those terms' pages. Served: unknown term is 200 with `missing_terms`. `datamk context --terms`: unknown term exits non-zero. |

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
| `datamk verify -p prod` | Live-checks schema and grain against the warehouse (native types where available, BigQuery today). Writes `.cell/source_check.json` with a `cell.yaml` digest. |
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
- `datamk release` digests `cell.yaml` prose and docs content; a change without a version bump warns. Warehouse prose is never in that digest.
- Interface digest = `/context` `ETag` = `/openapi.json` `info.version` = manifest `context_digest`. Prose and docs-content edits do not move it. Adding, removing, or renaming a `docs:` page does. `?include=docs` has its own `ETag` (`"<digest>~docs.<hash>"`); `If-None-Match` gives 304.
- Docs edits move `content_hash`, so a deploy rolls the workload.
- The Kubernetes ConfigMap carrying cell content is capped at 1 MiB, shared with `cell.yaml` and transform SQL.
- No caller-supplied SQL, filters, or projections on any route. Use `datamk attach` for SQL.
- `datamk mcp` serves this same document over stdio. See [mcp.md](mcp.md).

Design rationale: [ADR 0012](../adr/0012-cell-context-document.md),
[ADR 0013](../adr/0013-long-form-docs-pages.md),
[ADR 0015](../adr/0015-flat-context-document.md),
[ADR 0017](../adr/0017-definitions.md).
