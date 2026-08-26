# ADR 0016 — Discovered interfaces: SQLMesh as the first adapter

- **Status:** Accepted — implemented 2026-08-25.
- **Date:** 2026-08-24
- **Deciders:** Datamake team
- **Author:** @scottypate
- **Depends on:** ADR 0015 (flat context document, per-field provenance).
  Builds on ADR 0012 (bound exports, `binding`), ADR 0014 (`datamk.yaml`).

## Motivation

Requirements, verbatim from the founder:

- Developer-first, headless: CLI + API for agents. No UI, no chat.
- Reads a SQLMesh project's metadata; strictly read-only against SQLMesh.
- No double-entry: schema, grain, descriptions are never hand-copied or
  maintained in two places. Descriptions keep one home.
- Near-zero setup: point at the SQLMesh project once; models are discovered.
- Exposes only deployed reality, never undeployed local edits.
- Serving stays credential-light: no Python, no SQLMesh install, no
  warehouse credentials to serve.
- The design must extend to dbt behind the same surface.

Scope of this ADR is **catalog-only**: a SQLMesh project's deployed models
become exports an agent can discover and read about. Cells that *source*
from SQLMesh models by name are a later ADR, built on this one's reader.

### What the state store actually holds (verified)

Read directly with DuckDB against a `sqlmesh init` project, then against
mountain's production state (Cloud SQL Postgres, 2026-08-24 census):

- Six tables in schema `sqlmesh`: `_versions`, `_environments`,
  `_snapshots`, `_intervals`, `_environment_statements`,
  `_auto_restatements`. Blobs are `TEXT` JSON on every backend but MySQL.
- `_environments[prod]`: `plan_id`, `finalized_ts` (NULL transiently,
  while an apply is between promote and finalize — observed once on
  mountain's prod, which re-plans on every merge), `snapshots` — a JSON list of `{name, fingerprint{data_hash,
  metadata_hash, parent_data_hash, parent_metadata_hash}, version,
  kind_name, parents, change_category, physical_schema}`. No `identifier`.
- `_snapshots` PK is `(name, identifier)`; `(name, version)` is **not**
  unique (3.2× fan-out on mountain: 6379 rows for 1969 prod models).
  `identifier = str(zlib.crc32(";".join(the four hashes)))` — **decimal**,
  reproducible.
- `_snapshots.snapshot.node`: `description`, `owner`, `tags`, `cron`,
  `start`, `grains`, `references`, `audits`, `kind` (with `time_column`,
  `forward_only`…), `dialect`, `default_catalog`, `gateway`, the model SQL,
  and `mapping_schema` (the resolved column types of the model's
  *parents*). `columns`/`column_descriptions` exist **only when declared**
  in `MODEL(...)`; inline `--` comments stay inside the SQL text.
- `_intervals`: loaded `[start_ts, end_ts)` per `(name, version)`.
- Names are `"catalog"."schema"."table"` — double-quoted at every part on
  every dialect (BigQuery included), catalogs contain hyphens
  (`"dw-main-silver"`). `node.name` is the same name unquoted. A dev plan
  adds a snapshot row and a dev environment row; prod's row is untouched.
- `_versions`: `schema_version` (100), sqlglot version, sqlmesh version —
  mountain's is `0.0.1.dev4501`, a fork build.
- With `register_comments` (default on; confirmed on for mountain), SQLMesh
  writes model and column descriptions — declared *and* inline — onto the
  physical table and the prod view.

Mountain prod coverage, deduplicated to one row per model: 1969 models,
**920 (47%) `EXTERNAL`**; `columns` 51% (mostly the externals);
`description` 32%; `owner` 38%; `tags` 14%; `grains` 3.7%;
`column_descriptions` 3.1%. Env blob 3.8 MB; deduplicated prod snapshot
payload 13 MB; a full coverage scan took 3.7 s.

Three consequences drive the design: **selection is mandatory** (1969
exports is not an interface — VISION.md §"explicit export list"), **the
warehouse is the primary source of column definitions** (state has them for
a minority), and **`(name, crc32(fingerprint))` is the only correct join**.

## Decisions

### 1. A discovered cell: `discover:` instead of `interface:`

```yaml
cell: invoicing
description: Deployed invoicing models from the dw-main SQLMesh project.

discover:
  from: sqlmesh                 # closed set; `dbt` when its adapter lands
  environment: prod             # default prod — the deployed environment, never a dev env
  state: sqlmesh_state          # profiles/<p>.yaml connections.<name>: the state DB
  warehouse: dw_silver          # profiles/<p>.yaml connections.<name>: INFORMATION_SCHEMA reads
  select:                       # REQUIRED. No "everything" default. At least one of
    tags: [invoicing_step]      #   tags/schemas/models. OR within a key, AND across keys.
    schemas: [invoice]          #   (86% of mountain's models are untagged — schemas and
    models: [margins.flight_margin]   # explicit names are first-class, not fallbacks)
    kinds: [FULL, INCREMENTAL_BY_TIME_RANGE, VIEW]   # default: every kind except EXTERNAL and SEED
  exclude:
    models: [invoice.scratch_x]
  on_unresolvable: fail         # fail (default) | exclude — excluded models are named in notes[]
  overrides:                    # refine a discovered model; never invent one
    - model: invoice.flight_spend
      as: flight_spend
      version: 1.2.0            # authored. SQLMesh has no semver; see §6
      contract: supported
      grain: [month, campaign_group_id, flight_id]

access:
  shareable: true
  roles: [finance]
```

- `discover:` and any of `sources:`/`transforms:`/`interface:` together is
  a parse error: a discovered cell computes nothing and authors no list.
  `cell:`, `description:`, `access:`, `docs:` keep their meanings.
- `interface:` stays `Vec<Export>` (`src/config/schema.rs:34`). Discovery
  **materializes** that Vec — one bound `Export` plus one synthesized
  `Source::Connection { table }` per model — at exactly one place,
  `config::load`, so `context`, `serve`, `verify`, `release`, `openapi`
  iterate `def.interface` unchanged. No second route-derivation path (ADR
  0012 §4).
- Export name: `<schema>_<table>` lowercased, non-`[A-Za-z0-9_]` → `_`,
  validated by `is_valid_identifier` (`src/config/schema.rs:581`);
  collisions after mangling are a hard error naming both models and
  pointing at `overrides[].as`. The catalog is dropped from the name — it
  is the environment, and the same `cell.yaml` must mean the same thing
  against a staging state store.
- `select.kinds` excludes `EXTERNAL` (an `external_models.yaml` entry is an
  upstream, not a product) and `SEED` by default; both are selectable
  explicitly.
- Nothing from SQLMesh ever grants a datamk role, promotes a contract, or
  sets `visibility`. Promotion stays the one deliberate human gesture
  (`src/release.rs:10`).

### 2. `datamk sync` — a new verb; `run` is untouched

```
datamk sync -f cell.yaml -p prod [--dry-run]
```

`-f`/`-p` are `FileArgs` (`src/cli.rs:259`). `sync` reads the state store
and the warehouse, writes `.cell/deployed_catalog.json` (§5), and prints:

```
Discovered 86 models from sqlmesh environment 'prod' (plan 2c20ce12…, 41 selected)
  41 exports · 45 excluded by discover.select · 3 without descriptions
Wrote .cell/deployed_catalog.json
Next:
  datamk verify  -p prod    # live-check types against the warehouse
  datamk context -p prod    # the document agents read
  datamk serve   -p prod    # /context + /openapi.json; rows stay in the warehouse
```

Why not `run`: its refusal on transform-less cells
(`src/engine/mod.rs:654`) is the same predicate that makes the Kubernetes
target render no init Job (`render_init_job`); puncturing it forces a third
state into deploy rendering and gives `run` dead flags (`--full-refresh`,
`--verify-replay`, `--retention-days`). A read-only SELECT plus a JSON write
shares no code with bind → transact → snapshot.

Other verbs on a discovered cell:

| verb | behaviour |
|---|---|
| `run` | refuses: *this cell discovers its interface (`discover.from: sqlmesh`) and materializes nothing — use `datamk sync`* |
| `attach` | refuses: no datamk catalog holds rows; query the warehouse objects named in `exports[].binding.object` |
| `verify` | unchanged meaning — the existing bound-export live check (`src/verify.rs:320`) against every export; writes `.cell/source_check.json`; `status: verified_at_source` |
| `release` | pins `{plan_id, per-model fingerprint, interface_digest}` for `supported` exports; the description ratchet (`src/release.rs:73`) fires on authored prose only (ADR 0015 §6) |
| `rollback` | not applicable; refuses with a note that SQLMesh owns the objects |
| `status` | adds `source: sqlmesh (prod) · plan 2c20ce12… · synced 2026-08-24T19:02Z · 41 exports` |
| `serve` | unchanged flags; banner says *catalog only (41 bound exports, no data routes)* |
| `init <name> --from sqlmesh` | scaffolds §1 plus a non-empty profile `channels:` — the only field that tells a caller where rows actually are |

### 3. Reading the state store

- **Where:** a named entry in the profile's `connections:` map — never a
  new top-level profile key. Postgres via DuckDB's `postgres` extension
  (`ATTACH … (TYPE POSTGRES, READ_ONLY)`); a sqlite/duckdb file via
  `INSTALL sqlite` (already loaded at `src/engine/mod.rs:418-427`) or a
  direct read-only ATTACH. The state `schema` (default `sqlmesh`) is a
  connection field. No Python, no SQLMesh, no new Rust client.
- **Cloud SQL IAM auth is not in scope.** DuckDB's extension speaks libpq;
  an IAM token is a `${VAR}` password with an hour's life, or the Auth
  Proxy sits beside the job. `sync` runs and exits, so that is a job
  wrapper, not engine code. Stated in the guide, not solved here.
- **Exactly these reads:** `_versions` (one row); `_environments WHERE
  name = <environment>` (one row); `_snapshots WHERE identifier = ANY(<the
  crc32s computed from the env row>)` (N rows, PK-indexed — never a scan
  of the 25k-row table); `_intervals WHERE (name, version) IN (…) AND NOT
  is_dev AND NOT is_removed`. Read-only by construction: `sync` issues
  SELECTs only, and the ATTACH is `READ_ONLY`.
- **Join:** `(name, crc32(fingerprint))`, decimal-formatted
  (`crc32fast`). A unit test asserts every row of the fixture state DB
  round-trips and that the prod row for a model with a coexisting dev
  snapshot resolves to the prod fingerprint.
- **Names:** parsed by a quoted-identifier splitter (`"a"."b"."c"`, with
  `""` escapes), never `split('.')`. The virtual (prod) object is
  `<schema>.<table>` in `<catalog>`, from the environment entry's name —
  **never the physical `sqlmesh__<schema>.<schema>__<table>__<version>`**,
  which changes every plan. `catalog_name_override` (NULL on mountain) is
  applied when present; `gateway_managed` affects execution, not naming.
- **Pinning:** `_versions.schema_version` must be in a tested set (today
  `{100}`); mismatch is a hard error naming both numbers. The sqlmesh
  version string is **not** pinned (mountain runs a fork). Every blob field
  is read as `Option` through a `serde_json::Value` walk with a named error
  per missing required field (`name`, `fingerprint`, `version`,
  `kind_name`) — SQLMesh's blob shape moves without a `schema_version`
  bump (nine migrations in the trailing year), and a typed struct with
  required fields over another tool's private JSON would fail on the
  wrong day. A scheduled CI job replans the fixture project against
  latest SQLMesh and diffs the parsed IR.
- **An unfinalized environment is refused.** `finalized_ts` is NULL from
  `promote()` (which rewrites the row) until `FinalizeEnvironmentStage`
  confirms the view swap — i.e. while a prod apply is in flight, or after
  one died between the two. Mountain re-plans prod on every merge to
  main, so the window is routine and short. `sync` exits non-zero with
  *environment 'prod' is not finalized (plan <id> in flight) — retry* and
  a distinct exit code the scheduler can treat as "try again", rather
  than reading a virtual layer that may be mid-swap. The deployed stamp is
  therefore always `{plan_id, finalized_at}`, both present.

### 4. Column definitions: resolution order

Per selected model, in order; the first hit wins per *field*, and the
winning origin is recorded (`columns_source`, and `from` in the document):

| field | 1 | 2 | 3 |
|---|---|---|---|
| name, type | `node.columns` (declared) | warehouse `INFORMATION_SCHEMA` | — |
| description | `node.column_descriptions` (declared) | inline comments in `node.query`, by SQLMesh's own rule (below) | warehouse column comment |
| model description | `node.description` | warehouse table/view description | — |
| grain | `node.grains` | `overrides[].grain` | empty |

Steps 1 and 2 are both `from: "sqlmesh"` — the model definition is the
home of a description whether the author wrote it in `MODEL(...)` or as a
`--` comment. The warehouse copy (step 3) is SQLMesh's own rendering of
the same text under `register_comments`; it is the fallback for what the
extractor cannot resolve, never the primary.

**The inline-comment rule, replicated exactly** (`core/model/definition.
py:1485-1497`, verified against a fixture carrying every comment form):
when `column_descriptions` is not declared, a column's description is the
**last** comment attached to its projection in the **outermost** `SELECT`
of the model query — the left side of a set operation, after any CTEs.
sqlglot's attachment rule, which the Rust extractor reproduces: a comment
on the same line after a token attaches to the preceding projection
(across the comma); a comment on its own line attaches to the following
projection; `--` and `/* */` are equivalent; comments inside CTEs,
subqueries, or after the last projection attach to nothing. The column
name is the alias if present, else the bare column name. Projections the
extractor cannot name (macro-generated, `*`) fall to step 3.

The extractor is a tokenizer over strings, quoted identifiers, comments,
and paren depth — not a SQL parser — and it is held to a **differential
test**: SQLMesh's own `column_descriptions` for every model in the fixture
project, and a one-off dump of the same for every non-external model in
mountain's prod environment, must match the extractor's output exactly.
Any mismatch class is a bug in the extractor, not a tolerance.

The rule scrapes *any* adjacent comment, so some derived descriptions are
not descriptions (mountain: `"Add derived columns like PostgreSQL for
compatibility"` on a column). That is SQLMesh's behaviour — it registers
the same text on the warehouse — and datamk reproduces it rather than
curating it. Mountain today: 1049 non-external models, 130 with column
descriptions — 62 declared, 68 inline-only. The inline path is not a
corner case; it is half the coverage.

`mapping_schema` is **not** used for a model's own columns: it holds the
model's parents' types, and a child that happens to exist is not an
authority on its parent.

Warehouse reads are **batched per (connection, schema)**, not per model —
the existing `classify_one_dataset` shape (`src/engine/connectors/
bigquery.rs:100-132`, `ClassifyCache` in `connectors/mod.rs:70-165`).
Snowflake and Postgres get the same-shaped read; DuckDB falls back to
`DESCRIBE` per table. Cost is O(schemas) round trips, once, at `sync`.

A model with no resolvable columns (undeclared, and no `INFORMATION_SCHEMA`
row — typically not yet planned, or no metadata grant) is an error under
`on_unresolvable: fail` (default) naming the model and the three fixes, or
excluded and listed in `notes[]` under `exclude`. **Never emitted with a
fabricated type** (ADR 0012 §2).

`sync`'s summary counts descriptions by origin — declared, inline,
warehouse, none — so the operator sees where meaning actually lives; a
warning explains `register_comments` only when the warehouse step supplied
nothing at all.

### 5. The artifact: `.cell/deployed_catalog.json`

A sidecar on the `SourceDescriptionsRecord` pattern (`src/manifest.rs:
186-300`): `{written_at, datamk_version, cell_yaml_digest, profile,
catalog: DeployedCatalog}`. Written by `sync` (has state + warehouse
credentials); read by `context`, `serve`, `verify`, `release` with **no
credentials** — the same seam `.cell/source_check.json` already crosses,
folded into `deploy::artifact::collect` (`src/deploy/artifact.rs:68-127`)
so a fresh sync reaches a pod by rollout.

**Not** a published execution: that layout hard-codes `.ducklake`
(`src/store.rs:485/518/543`), `setup` ATTACHes whatever `LATEST` names
(`src/engine/mod.rs:361-369`), and `attach`/`rollback`/`status` assume it.
Two artifact kinds in a layout that knows one is the wrong trade.

**Freshness.** `fresh_for` keys on `(cell_yaml_digest, profile)` and
nothing else — see the 2026-08-26 amendment, which removed the `max_age`
clock this section originally carried. `serve` **refuses to start** on a
missing or invalidated record — never an empty interface with a valid ETag
(ADR 0014 §6, startup is strict); `context` emits `status: draft` with an
engine note naming why.

The staleness window is a named property of the design: a warehouse object
altered outside a SQLMesh plan is invisible until the next `sync` or
`verify`. This replaces, for discovered cells, the live-check-every-run
guarantee hand-authored bound exports have — stated, not implied.

### 6. Versions and contracts

SQLMesh has fingerprints, not semver, and `Export::major()` requires
semver (`src/config/schema.rs:126`). Deriving a route key from a
fingerprint would move public routes on a whitespace edit; deriving it from
`change_category` would let a third party's classifier move datamk's
contract with no human gesture.

- Discovered exports are `version: 1.0.0`, `contract: experimental`, route
  `name@1`, and the document says they are unpinnable.
- `contract: supported` requires an `overrides[]` entry naming the model
  and an authored `version`. A few lines for the exports anyone depends on;
  not double-entry for the rest.
- `sync` **refuses to overwrite** the record when a `supported` model's
  `data_hash` has moved since the last sync and its authored `version` has
  not: the upstream changed a contract datamk promised. The operator bumps
  `version`, or excludes the model. Fingerprints never appear in `version`
  or in the digest; they live in the measured `deployed` block (§7).

### 7. The document (on ADR 0015's flat shape)

Additive fields only; `datamk_context` stays at 4.

```json
{
  "datamk_context": 4,
  "cell": "invoicing",
  "status": "verified_at_source",
  "discovered_from": { "tool": "sqlmesh", "environment": "prod",
                       "plan_id": "ea22e379d0ca448e9ee95df88eb406f5",
                       "finalized_at": "2026-08-24T22:51:46Z", "synced_at": "2026-08-25T04:02:11Z",
                       "evidence": "environment_row" },
  "exports": [{
    "name": "flight_spend", "version": "1.2.0", "route": "flight_spend@1",
    "contract": "supported",
    "description": "Spend by flight_id and month for invoicing and UI reporting",
    "grain": ["month", "campaign_group_id", "flight_id"],
    "from": { "description": "sqlmesh", "grain": "cell.yaml" },
    "schema": {
      "month":          { "type": "DATE",    "from": { "type": "warehouse" } },
      "invoice_amount": { "type": "NUMERIC", "description": "…",
                          "from": { "type": "warehouse", "description": "warehouse" } }
    },
    "query": null,
    "binding": { "source": "flight_spend",
                 "object": "invoice.flight_spend", "connection": "dw_silver" },
    "depends_on": ["public_advertisers@1", "ui_ui_flights@1"],
    "depends_on_unselected": 2,
    "deployed": { "at": "2026-08-24T19:02:11Z",
                  "kind": "INCREMENTAL_BY_TIME_RANGE", "cron": "@hourly",
                  "owner": "ber", "tags": ["invoicing_step"],
                  "fingerprint": "3237974788", "version": "3936512190",
                  "intervals": { "start": "2000-01-01", "end": "2026-08-24T18:00:00Z" },
                  "pending_restatement": false }
  }],
  "data": { "served_here": false, "channels": ["Rows live in BigQuery dw-main-silver; go/data-access"] }
}
```

- Everything SQLMesh declared lands in the interface fields with `from:
  "sqlmesh"`; everything the warehouse supplied, `from: "warehouse"`;
  overrides, `from: "cell.yaml"`. Precedence per ADR 0015 §2.
- `deployed` is a **measured** block (it has `at`), so it is outside the
  digest: an upstream tag or cron edit never moves the ETag. `description`
  and `schema` are interface fields, so an upstream *meaning* change does —
  correctly.
- Lineage: `depends_on` lists selected parents by route key;
  `depends_on_unselected` is a count, never the names (the unselected
  models are not this cell's to disclose — ADR 0012 §5). It does not reuse
  `upstreams[]`, which means a *cell* dependency.
- `query: null` and `binding` present — the bound-export shape (ADR 0012),
  so `/openapi.json` carries `/context` and `/openapi.json` only and a data
  route returns the existing bound-export 404 (`src/serve/mod.rs:1467`).
- `status` follows the existing rules: `draft` until `verify` has
  live-checked the exports (`verified_at_source`); never `verified`, which
  means a published execution. `grain_verified` is `true` only for exports
  whose grain `verify` actually measured.

### 8. The adapter seam: `DeployedCatalog`

`src/catalog/{mod,ir,sqlmesh}.rs`. Everything downstream of `ir.rs` is
tool-agnostic.

```rust
pub struct DeployedCatalog {
    pub tool: String,                  // "sqlmesh" | "dbt"
    pub environment: String,
    pub plan_id: String,               // dbt: invocation_id
    pub finalized_at: String,          // sqlmesh: refused when NULL (§3); dbt: artifact generated_at
    pub synced_at: String,
    pub evidence: Evidence,            // EnvironmentRow | ArtifactOnly
    pub schema_version: String,        // the tool's own state/artifact version
    pub models: Vec<DeployedModel>,
}
pub struct DeployedModel {
    pub name: String,                  // unquoted catalog.schema.table
    pub object: String,                // schema.table — the virtual object, never physical
    pub catalog: Option<String>,
    pub fingerprint: String,           // opaque change token; sqlmesh: identifier
    pub version: Option<String>,       // sqlmesh: data version; dbt: none
    pub kind: String,
    pub cron: Option<String>,
    pub owner: Option<String>,
    pub tags: Vec<String>,
    pub description: Option<String>,
    pub description_source: Option<Origin>,
    pub columns: IndexMap<String, DeployedColumn>,
    pub columns_source: Origin,        // Declared | Warehouse
    pub grain: Vec<String>,
    pub depends_on: Vec<String>,       // unquoted model names
    pub intervals: Option<Interval>,   // dbt: None
    pub pending_restatement: Option<bool>,
}
```

**dbt fit, stated now so the seam is honest:** `manifest.json` +
`catalog.json` give `unique_id`, `depends_on.nodes`,
`config.materialized`, `columns` with descriptions, a file `checksum` (the
opaque fingerprint), and `metadata.invocation_id`. They give **no
environment row** — a laptop's dev artifacts are byte-shaped like prod's.
So dbt's `evidence` is `ArtifactOnly`, surfaced verbatim in
`discovered_from.evidence`; the requirement "never undeployed local edits"
is met for SQLMesh by construction and for dbt only as strongly as the
operator's artifact pipeline. `intervals` is `None`. Nothing in
`DeployedModel` is `Option` because dbt lacks it *except* those two fields.

### 9. Many discovered cells, one project

A SQLMesh project of 1969 models becomes several cells — one per `select`
(a tag, a schema, a team) — composed in `datamk.yaml` (ADR 0014) and
listed in a mesh manifest like any other cells. Each is narrow, owned, and
independently promotable. The discovery reader is shared; the interfaces
are not. This is the export-list discipline applied to someone else's
DAG, not a catalog of it.

### 10. Out of scope, deliberately

- **MCP.** The repository has no MCP server. `/context` is already the
  resource an MCP adapter would expose; a stdio `datamk mcp` with two tools
  (`list_exports`, `get_export`) is its own small ADR.
- **`sqlmesh:` sources** for cells that build on SQLMesh models — phase
  two, on this reader.
- Reading model SQL, audits, or `mapping_schema` into the document. SQL is
  the upstream owner's private seam; audits are not verification datamk
  performed.

## Consequences

- Near-zero setup holds for browsing (`discover` + one `select`); contracts
  cost one override line each.
- A discovered cell has a staleness window (§5) instead of a live check;
  `verify` on a schedule closes it to the schedule's period.
- Two new profile connection uses (state, warehouse), no new top-level
  profile keys, one new verb, one new sidecar, one new module tree.
- Blast radius on hand-authored cells: additive `Option` fields in
  `deploy::artifact::collect`, `serve`'s startup block, `context::
  build_document`; the `interface` materialization point in
  `config::load`; nothing in the execution layout.
- Sequencing: (1) ADR 0015 lands; (2) `catalog::ir` + `catalog::sqlmesh`
  with the fixture state DB (`sqlmesh init` duckdb project, checked in) and
  the identifier/join/quoting tests; (3) `sync` + sidecar + freshness; (4)
  `discover:` materialization in `config::load` + verb refusals; (5)
  document fields + `verify`/`release` behaviour; (6) run against
  mountain: one cell, `select.tags: [invoicing_step]`, and record model
  count, bytes, and `sync` wall-clock.

## Premises — what this rests on, and what would reverse it

1. **`_environments[prod]` changes only through a plan.** Falsifier: a
   restatement or forward-only path mutates the prod row's snapshot list
   without a new `plan_id`. Check: diff the row across a week of mountain
   operations.
2. **An unfinalized environment is a transient state, and refusing it is
   cheap.** Verified on mountain: NULL appears only between promote and
   finalize (`state_sync/db/facade.py` rewrites the row; the finalize
   stage stamps it last), and the next apply self-heals by re-promoting
   everything. Falsifier: an estate whose prod applies fail after promote
   often enough that `sync` rarely finds a finalized row; then the retry
   policy needs a ceiling and an alert, not a different read.
3. **Selection yields cells of a size an agent can hold.** Verified:
   mountain's largest tag selects 50 models, `invoicing_step` selects 5 —
   but 86% of prod models carry no tag, so `schemas`/`models` do most of
   the selecting. Falsifier: a schema-based selection routinely exceeds
   ~100 exports; then the guide recommends splitting by `models`, and
   near-zero setup weakens for that estate.
4. **The warehouse read supplies column definitions for the selected
   models** (`register_comments` on; the sync identity has metadata read on
   every selected schema). Falsifier: `INFORMATION_SCHEMA` on the prod
   views has no descriptions for a meaningful share of selected models;
   then descriptions are an upstream authoring problem and the document
   says so, but the product is thinner than promised.
5. **The blob shape stays parseable across SQLMesh releases with `Option`
   reads and a `schema_version` pin.** Falsifier: a release renames
   `fingerprint` or `snapshots`; the CI replan job catches it before a
   user does.
6. **`supported` sets are small.** Falsifier: an estate needs hundreds of
   pinned discovered exports; then per-export overrides are double-entry
   and versioning needs a different answer (e.g. a version *policy* in
   `discover:`).
7. **The inline-comment extractor matches SQLMesh on real models.**
   Verified 2026-08-25: 977 mountain models, 0 mismatches (after three
   fixes the first run surfaced — `* EXCEPT (…)` star modifiers, `[…]`
   subscripts, and keyword aliases like `AS time`). Falsifier: a future
   SQLMesh/sqlglot release changes comment attachment; the fixture
   differential and `datamk debug sqlmesh-comments` re-run against the
   real project are the check.
8. **Agents route on the document without warehouse credentials** — the
   founder's requirement, and the reason `INFORMATION_SCHEMA` is not a
   competitor. Falsifier: the target agents all hold warehouse access; then
   the delta shrinks to lineage, intervals, plan pinning, and grain, and
   the artifact is still worth having but the cell wrapper is not.

## Amendment (2026-08-26): the first production run

Six change requests from running `0.0.20` against two SQLMesh projects
(the 1,969-model monorepo and a 93-model project; cross-catalog selection,
Cloud SQL state through the Auth Proxy). Landed in `0.0.21`:

1. **`verify` reads each export from the model's own catalog.** `sync`
   already did; `verify`'s bind path classified everything against the
   connection's project. A discovered export's synthesized source now
   carries `project.dataset.table` when the warehouse is BigQuery, and the
   BigQuery connector accepts a three-part path: classified in its own
   project's `INFORMATION_SCHEMA`, billed to the connection, and always
   read through the jobs API (the attach is one project). `binding.object`
   is therefore project-qualified on BigQuery — the verbatim source, as
   ADR 0012 requires.
2. **`overrides[].docs`** — the one authored thing per export with no home
   upstream (ADR 0013 rules unchanged; validated after materialization).
3. **`select` is AND across keys**, as §1 always said; the code OR'd them
   and over-selected silently into other catalogs.
4. **No storage for a cell that materializes nothing.** `verify`,
   `context` and `serve` on a no-snapshot cell register no store secrets,
   attach no catalog, and never touch `storage:` — so a `gs://` profile
   needs no HMAC pair and a local one no `catalog:`.
5. **The record refreshes on deploy, and only on deploy — `max_age` is
   removed.** The deploy is downstream of `sqlmesh plan`: `plan prod` →
   `sync` → `deploy`. The record is as current as the last deploy by
   construction, `discovered_from.{plan_id, synced_at}` name the plan, and
   a clock could only produce false alarms. `deploy` renders a Server only
   for a no-snapshot cell (no Builder CronJob, no init Job). The polling/
   runtime-reload shape (a sync CronJob writing to storage, `serve`
   swapping the interface) was considered and rejected: it is a second
   refresh path for a cell whose interface is supposed to move exactly
   when the upstream deploys.
6. Guide: Cloud SQL recipe promoted to a section; the emitted document has
   no `query` key on a bound export; docs on discovered cells; the scaffold's
   `gs://` profile no longer implies `gcs:` credentials.

What held without change: cross-catalog sync through one connection,
IAM-token state attach through the proxy, provenance (191/191 columns from
the tool on the smaller project; 52/55 honestly `none` on the monorepo
slice), in-cell lineage, and the refusal semantics.

