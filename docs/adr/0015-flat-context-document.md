# ADR 0015 — A flat context document with per-field provenance

- **Status:** Accepted — implemented 2026-08-25.
- **Date:** 2026-08-24
- **Deciders:** Datamake team
- **Author:** @scottypate
- **Supersedes:** ADR 0012 §2's `declared`/`observed` regions. Everything
  else in ADR 0012 — projection not product, one document one route, the
  meaning fields and their ratchet, provenance admitted to the wire, the
  disclosure boundary — stands.

## Motivation

ADR 0012 §2 made `declared` and `observed` "structurally separate regions,
never flattened," so that an agent could never mistake a claim for a
measurement. The guarantee is right. The mechanism is wrong, and the SQLMesh
integration (ADR 0016) is the case that shows it:

1. **Provenance is a property of a fact, not of a region.** A column's
   description can come from `cell.yaml`, from the warehouse's own column
   comment, or — once an interface is discovered from a modeling tool — from
   that tool's model definition. Today the first lives at
   `declared.exports[].schema.<col>.description` and the second at
   `observed.source_descriptions.<source>.<col>` (`src/context.rs:339`):
   the same fact with two homes, keyed two different ways, and an agent has
   to know both to answer "what does this column mean." That is the
   double-entry the product exists to remove.
2. **A discovered interface has no author.** When every export comes from
   SQLMesh's deployed state, "author claim" describes nothing. The review of
   ADR 0016 had to choose between filing tool-declared prose under
   `observed` (a misnomer — SQLMesh *declared* it) or under `declared` (which
   churns `interface_digest` on every upstream plan). Both answers are
   artifacts of partitioning by region.
3. **The digest is coupled to the block structure.** `interface_digest`
   serializes `Declared` whole (`src/context.rs:997-1013`), so *where* a
   field sits decides whether it moves the ETag — which is why
   `ObservedUpstream.execution` had to be split from `UpstreamRef`
   (`src/context.rs:349`) and docs fingerprints from docs identity: three
   pairs of structs that describe one thing each.

The founder's position, adopted here: one document, one home per fact, and
the fact says where it came from.

## Decisions

### 1. `datamk_context: 4` — one level, no regions

The document is flat. Every field that ADR 0012 placed under `declared` or
`observed` moves to the top level or onto the record it describes. The
claim-vs-measurement guarantee is carried by **provenance on the fact**
(§2) and by timestamps on every measured block (§3), not by position.

```json
{
  "datamk_context": 4,
  "cell": "orders",
  "status": "verified",
  "grain_verified": true,
  "description": "Daily order revenue by region.",
  "from": { "description": "cell.yaml" },

  "exports": [{
    "name": "orders_daily", "version": "2.1.0", "route": "orders_daily@2",
    "contract": "supported",
    "description": "One row per (order_date, region) with the summed order revenue.",
    "grain": ["order_date", "region"],
    "from": { "description": "cell.yaml", "grain": "cell.yaml" },
    "schema": {
      "order_date": { "type": "date",
                      "from": { "type": "cell.yaml" } },
      "revenue":    { "type": "decimal", "unit": "USD",
                      "description": "Gross order revenue, before refunds.",
                      "from": { "type": "cell.yaml", "description": "warehouse" } }
    },
    "query": { "filters": ["order_date", "region"], "filter_semantics": "…",
               "limit_default": 100, "limit_max": 1000, "offset_max": 1000000,
               "sample_request": "orders_daily@2?limit=10" },
    "probe": { "at": "2026-08-06T10:00:05Z", "rows": 4,
               "coverage": { "order_date": { "min": "2026-06-01", "max": "2026-06-02" } },
               "values": { "region": { "values": ["eu-west", "us-east"], "complete": true } },
               "example_request": "orders_daily@2?order_date=2026-06-01&region=us-east&limit=10" }
  }],

  "upstreams": [
    { "ref": "flights", "version": null, "execution": 41, "data_as_of": "2026-08-04 06:00:11+00" }
  ],
  "docs": [
    { "target": "cell", "source_path": "docs/cell.md", "media_type": "text/markdown",
      "sha256": "…", "bytes": 4120 }
  ],
  "include_request": "context?include=docs",

  "build": { "execution": 47, "snapshot_id": 12, "verify_outcome": "passed",
             "started_at": "…", "finished_at": "2026-08-06T10:00:05Z",
             "datamk_version": "0.9.0", "data_as_of": "2026-08-06 10:00:04+00" },
  "data": { "served_here": true, "channels": [] },
  "included": [],
  "notes": []
}
```

Field moves, exactly:

| ADR 0012 (v3) | v4 |
|---|---|
| `declared.description` | `description` + `from.description` |
| `declared.exports[]` | `exports[]` |
| `declared.exports[].schema.<col>` | `exports[].schema.<col>` + `from` |
| `observed.source_descriptions.<source>.<col>` | `exports[].schema.<col>.description` with `from.description: "warehouse"`, **bound exports only** (§4) |
| `observed.exports.<route>` (probe) | `exports[].probe` + `at` |
| `observed.source_check.exports.<route>` | `exports[].check` + `at` |
| `declared.upstreams[]` + `observed.upstreams[]` | one `upstreams[]` record: `{ref, version, execution, data_as_of}` |
| `declared.docs[]` + `observed.docs.<target>` + top-level `docs` | one `docs[]` record: identity, fingerprint, and (under `?include=docs`) `content` |
| `declared.include_request` | `include_request` |
| `observed.provenance` | `build` |
| `observed.source_check` (cell-level fields) | `source_check` |
| `observed.freshness` | `freshness` (hosted, published mode only — unchanged) |

Unchanged: `status`, `grain_verified`, `data`, `notes`, `included`,
`emitted_at`, `cell_yaml_digest`, and every rule ADR 0012 attached to them
(absent facts omitted or `null`, never fabricated; unbuilt cells assert
`draft` positively; `notes[]` engine-emitted only).

`observed: null` on an unbuilt cell had one job — say positively that
nothing has been measured. `status: "draft"` already says it, and `build`,
`source_check`, `probe`, `check` are each absent (never `{}`) on such a
cell. The invariant survives without the wrapper.

### 2. Provenance is a `from` map on the record

Every record whose fields can originate in more than one place carries
`from: { <field>: <origin> }`, naming **every** such field, always — an
agent reads `from.description` to learn who said it, and a reader who
doesn't care reads the document as plain JSON because values stay bare.

`origin` is a closed set: `cell.yaml` | `warehouse`, extended by exactly
one token per modeling-tool adapter as its ADR lands (`sqlmesh` in ADR
0016; `dbt` later). Not free text — an origin an agent can't recognise is
a bug, and `deny_unknown_fields`-style strictness applies to consumers.

Fields that carry provenance today: cell `description`; export
`description`, `grain`; column `type`, `unit`, `description`. Fields with
exactly one possible origin (`name`, `version`, `route`, `contract`,
`query`, `binding`, `freshness`) carry none.

**Precedence when a field has more than one candidate:** `cell.yaml` >
tool > `warehouse`. An author who writes a description means something
different from the upstream's words (the `interface import` rule,
`src/interface.rs:1-13`); a tool's declaration outranks a comment the tool
itself registered on the warehouse object. The losing candidates are not
emitted — one home per fact.

### 3. Measured facts carry `at`; that is what makes them measurements

`build`, `source_check`, `freshness`, `exports[].probe`, `exports[].check`
and `upstreams[].{execution, data_as_of}` are measurements. Each block
carries its own timestamp (`finished_at`, `checked_at`, `polled_at`,
`at`), and each is absent when nothing measured it. ADR 0012 §5's admitted
fields, its never-on-the-wire list, and the swap-time-never-request-path
rule are unchanged; only the address changed.

The claim-vs-measurement guarantee restated for v4: **a fact is a
measurement iff it sits in a block with a timestamp; a fact is a claim iff
it carries `from`.** Nothing is both, nothing is neither.

### 4. Warehouse descriptions land on bound exports' columns, and nowhere else

`observed.source_descriptions` was keyed by `sources:` name. A bound
export's columns *are* its source's columns, so the warehouse comment for
`revenue` is the description of the export's `revenue`, and it lands there
(`from.description: "warehouse"`, unless `cell.yaml` wrote one).

For a materialized export there is no such identity — its columns are the
output of transforms, and a source column called `revenue` says nothing
reliable about an export column called `revenue`. Those descriptions are
**dropped from the document**. They were also the one place a `sources:`
name crossed the private/public seam ADR 0012 §5 draws; v4 closes it. An
author who wants upstream meaning on a transformed column writes it (or
`interface import`s the source and binds it). This is a removal; see
"What would reverse this."

### 5. The digest is an explicit projection, not a region

`interface_digest` hashes a purpose-built `InterfaceProjection` struct:
`datamk_context`, `cell`, `description`, `exports[].{name, version, route,
contract, description, freshness, grain, schema.<col>.{type, unit,
description}, query, binding}`, `upstreams[].{ref, version}`, `docs[].{target,
source_path, media_type}`, `data`. Nothing else: not `from`, not any
timestamped block, not `notes`, not docs content or fingerprints.

Properties preserved, now by construction rather than by placement:
- A data refresh moves nothing in the projection ⇒ ETag stable.
- A docs prose edit moves nothing ⇒ ETag stable; page add/remove/rename
  does move it (ADR 0013).
- A mount path appears nowhere ⇒ same `cell.yaml`, same digest, anywhere
  (ADR 0014 §3).
- A description whose *text* is unchanged but whose *origin* changed does
  not move it — `from` is excluded on purpose.

One property changes, deliberately: a warehouse comment edit on a bound
export's column **does** move the ETag (its text is in the projection).
That is the interface as an agent experiences it changing, and the ETag's
job is to say so. Issue #10's concern was a different gate — see §6.

### 6. The release ratchet keys on authored prose only

`release`'s description ratchet (`src/release.rs:73-94`) compares
`description_digest` across pins and warns on a meaning change under an
unchanged version. In v4 it hashes exactly the fields whose `from` is
`cell.yaml` (plus docs content, as today). An upstream comment edit
therefore still cannot move datamk's own release gate — issue #10's
decision is preserved — while the same edit *is* visible to consumers via
the ETag (§5). Two gates, two questions: "did the author change the
meaning?" and "did the interface an agent reads change?"

The `supported ⇒ description` lint (`src/verify.rs:562-604`) keeps
accepting a warehouse-documented column, as it does today.

### 7. Consumers and compatibility

- `datamk_context` bumps 3 → 4 so a consumer can tell the shapes apart.
  No deprecation window and no side-by-side serving of v3: there are no
  production consumers of the document yet, so the cost of a window is all
  code and no protection. The CHANGELOG entry is **BREAKING** and names
  every moved path from the table in §1.
- The mesh emitter (`src/mesh.rs:229-269`) reads `description` and
  `exports[]` from the top level and accepts `datamk_context` 1–4. The mesh
  manifest's own shape (`datamk_mesh: 1`) is unchanged — it copies nothing
  that moved.
- `/openapi.json` `info.version` remains the digest.
- The golden serialization test in `src/context.rs` is rewritten for v4;
  `docs/guides/context.md` is rewritten around the flat shape and the
  `from` / `at` rule of §3.

## Consequences

- Every column description an agent reads has one address and says who
  wrote it. `interface import` and ADR 0016's discovery both emit into the
  same fields with a different `from`, instead of a second mechanism.
- Three struct pairs collapse: `UpstreamRef`/`ObservedUpstream`,
  `DeclaredDocsEntry`/`DocsFingerprint`(+content), the export probe/check
  maps keyed by route → fields on the export they measure.
- The document gets slightly wider per record (`from`, `at`) and loses two
  nesting levels. Anything scripting `declared.*`/`observed.*` (today: the
  mesh emitter, the guide, the golden test) is updated in the same PR.
- Source-keyed warehouse descriptions for materialized exports are gone
  (§4).
- One-PR change: `context.rs` (structs, builders, digest projection, golden
  test), `manifest.rs` (`SourceDescriptionsRecord` consumers), `release.rs`
  (ratchet field selection), `mesh.rs` (paths + version allowlist),
  `serve/mod.rs` (ETag/probe wiring), `verify.rs` (check emission),
  `docs/guides/context.md`, CHANGELOG. ADR 0016 lands on top of it.

## What would reverse this

- A design partner's agent demonstrably reads provenance better from
  position than from a `from` field — i.e. after upgrade it starts
  treating warehouse or tool prose as authored truth in a way v3 prevented.
  The fix then is a stronger `from` (e.g. wrapping values), not regions.
- Materialized-export authors turn out to depend on source-keyed warehouse
  descriptions (§4). Reversal is additive: an `inputs[]` block keyed by a
  *public* alias, never the `sources:` name.
- The ETag moving on warehouse comment edits (§5) proves to churn agent
  caches at a rate that matters. Reversal is a projection change (exclude
  `from != cell.yaml` descriptions from the digest) — one struct, no
  re-shape.
- The closed `origin` set proves too coarse (an agent needs "which
  warehouse" or "which tool version"). Reversal is additive: `from` values
  become objects; the map shape stays.
