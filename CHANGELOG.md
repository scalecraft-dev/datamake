# Changelog

All notable user-visible changes to `datamk` are recorded here. Format
loosely follows [Keep a Changelog](https://keepachangelog.com/); dates are
`YYYY-MM-DD`.

## [Unreleased]

### Changed — `freshness` documented as advisory (issue #11)

An export's `freshness:` is the author's intended cadence and nothing
checks it: not `verify`, not the Server, and nothing in the context
document contradicts it when the data is stale. The field's doc comment,
the context guide, and `/openapi.json` now say so, and point at
`build.data_as_of` for what is measured. The document's top-level
`freshness` block is unrelated Server poll telemetry, also now labeled as
such. Observed freshness (comparing the claim against data age) is split
out as its own issue.

### Added — `null_rows` on `exports[].check` and `exports[].probe` (issue #10, ADR 0012 amendment 2026-09-05)

`verify` now counts, per grain column, the rows with a NULL there, in the
same pass as the uniqueness check; the probe measures the same against the
rows each route serves. A NULL grain value never fails `verify` — the grain
can still be unique — but no equality filter can reach the row, so `verify`
warns, naming the column and the coalesce fix, and the count is published
on both blocks in `/context` (every grain column, zeros included; absent
only on a check record written by an older datamk). A uniqueness failure
whose grain has NULLs now states the NULL count as a fact beside the
existing replay-safety hint, and says plainly that a sentinel will not
make those rows unique.

### Added — `GET /context/{route}` and `datamk context --export` (ADR 0012 amendment 2026-08-27)

One export's slice of the context document: same schema, `exports[]` of
length one, `docs[]` reduced to the cell page and that export's; own ETag
variant; `?include=docs` as on `/context`; 404 names the real routes;
advertised in `/openapi.json` with `route` enumerated.

### Added — container-aware DuckDB memory default

Inside a cgroup, DuckDB's `memory_limit` defaults to 75% of the container's
limit (logged at start-up); `DATAMK_MEMORY_LIMIT` still overrides. The
release image bakes every extension the engine may `INSTALL`
(`datamk debug install-extensions`).

### Changed — discovered cells: `discover.on_missing_override: warn | fail`

An override whose model is gone or unselected warns by default, naming the
model and what will not appear; `fail` refuses the sync.

### Added — `serve` shuts down gracefully on SIGTERM/SIGINT

Stops accepting, drains in-flight requests for up to `--drain-timeout`
seconds (default 10), exits 0; one log line on receipt and one on exit. A
container with `datamk serve` as PID 1 previously ignored SIGTERM and was
SIGKILLed after the full grace period — mid-request — on every rollout.

### Fixed — discovered cells, 0.0.22 re-test

- **`verify` failed on every discovered cell** ("names project … cannot
  read through the Storage Read API … internal error"): the BigQuery
  connector's `qualify` refused the project-qualified sources 0.0.21
  introduced, before classification ever ran. A path naming the
  connection's own project is the attach read; another project is the
  jobs-API read, billed to the connection — in `qualify` itself, so every
  caller gets a working relation.
- An override's `docs:` page is validated at `sync` (path rules, caps),
  not first at `context`/`serve`.
- Profiles are a closed shape: an unknown key such as the removed
  `discover.max_age` is a parse error, not silently ignored.
- `deploy` ships a digest-gated sidecar (`source_check`,
  `source_descriptions`, `deployed_catalog`) only when it attests the
  current `cell.yaml`; a stale one no longer rides the ConfigMap.
- Guide: one cell per modeling project; "stale" wording; docs validation
  verbs.

### Changed — discovered cells, after the first production run (ADR 0016 amendment 2026-08-26)

- `verify` now reads each discovered export from the model's own catalog:
  on BigQuery the synthesized source is `project.dataset.table`, classified
  in that project and billed to the connection. The BigQuery connector
  accepts three-part paths on any `table:` source (jobs-API read).
- `discover.overrides[].docs` — a per-export docs page (ADR 0013 rules).
- `discover.select` is AND across keys (tags/schemas/models), as documented;
  it was OR, over-selecting silently.
- `verify`, `context`, `serve` on a cell with no materializing transforms
  never touch `storage:` — no object-store credentials, no `catalog:`
  needed; no "no catalog to attach yet" log line.
- **Removed** `discover.max_age` (profile) and the record's expiry: a
  discovered cell's record refreshes on deploy (`sqlmesh plan prod` →
  `datamk sync` → `datamk deploy`) and is as current as the last deploy by
  construction. `deploy` renders no Builder CronJob for such a cell.
- Document: a bound export has no `query` key (was documented as `null`);
  `binding.object` is project-qualified on BigQuery.

### Added — discovered cells: a SQLMesh project's deployed models as an interface (ADR 0016)

`discover:` in `cell.yaml` points a cell at a SQLMesh project; `datamk sync`
reads the deployed environment from the tool's own state store (read-only
`SELECT`s, no Python, no SQLMesh install) and the warehouse's
`INFORMATION_SCHEMA`, and writes `.cell/deployed_catalog.json`. Every
selected model becomes a bound export with the model's own schema, grain
and descriptions (declared, inline-comment, or warehouse-registered — each
marked with its origin), lineage inside the cell, and a per-export
`deployed` block (kind, cron, owner, tags, fingerprint, loaded intervals).
`verify`, `context`, `serve` and `deploy` then need no credentials.

- `datamk sync [-f cell.yaml] [-p profile] [--dry-run]` — new verb. Exit
  code 75 when the environment is mid-apply (retry).
- `datamk init <name> --from sqlmesh` — scaffolds a discovered cell.
- `select` is required (tags/schemas/models; `EXTERNAL` and `SEED` excluded
  unless asked); `overrides` refine a model (`as`, `version`, `contract`,
  `grain`, `description`, `visibility`); `on_unresolvable: fail | exclude`.
- Discovered exports are `1.0.0`/`experimental`; promotion needs an authored
  `version`, and `sync` refuses to overwrite a `supported` model whose data
  definition moved upstream under an unchanged version.
- `serve` refuses to start on a missing or stale record (profile
  `discover.max_age`, default 48h); `run`, `attach`, `rollback` refuse a
  discovered cell; `status` reports the plan and sync time.
- Document: top-level `discovered_from`; per export `deployed`,
  `depends_on`, `depends_on_unselected`; `from` gains the `sqlmesh` origin.
- `datamk debug sqlmesh-comments <file>` (hidden) — the differential check
  for the inline-comment extractor against SQLMesh's own output.
- `.cell/deployed_catalog.json` ships in the deploy artifact and rolls the
  workload like `published.json`.

See `docs/guides/discover.md`.

### Added — `type: duckdb` profile connections

A DuckDB database file as a warehouse connection: attached read-only, every
object read-through, with native types and registered comments supplied
from `duckdb_columns()` — so `verify`'s type authority and warehouse
descriptions work against a local file (a SQLMesh project's default engine
and state store are exactly this). Usable by any `sources:` entry, not only
discovered cells.

### Changed — BREAKING: the context document is flat, with per-field provenance (`datamk_context: 4`)

ADR 0015. The `declared`/`observed` regions are gone. Every field that lived
under them is top-level or on the record it describes, and every fact says
where it came from: a record that carries `from: { <field>: <origin> }` is a
claim (`cell.yaml` | `warehouse`); a block with a timestamp is a
measurement. One home per fact.

Moved paths:

| v3 | v4 |
|---|---|
| `declared.description` | `description` (+ `from.description`) |
| `declared.exports[]` | `exports[]` (each with `from`; columns with `from`) |
| `declared.upstreams[]` + `observed.upstreams[]` | one `upstreams[]` record: `{ref, version, execution, data_as_of}` |
| `declared.docs[]` + `observed.docs.<target>` + top-level `docs` | one `docs[]` record: identity + `{sha256, bytes}` + `content` (under `?include=docs`) |
| `declared.include_request` | `include_request` |
| `observed.provenance` | `build` (absent, never `null`, when no execution stands behind the document) |
| `observed.exports.<route>` (probe) | `exports[].probe` (+ `at`) |
| `observed.source_check.exports.<route>` | `exports[].check` (+ `at`) |
| `observed.source_check` | `source_check` (cell-level fields only) |
| `observed.freshness` | `freshness` |
| `observed.source_descriptions` | **removed** — warehouse column comments land on a *bound* export's own columns with `from.description: "warehouse"`; materialized exports no longer carry source-keyed descriptions |

`interface_digest` (the `ETag`, `/openapi.json`'s `info.version`, the mesh
`context_digest`) now hashes an explicit projection: it still ignores every
measurement and docs content, still ignores `from`, and now **does** move
when a warehouse column comment on a bound export changes — that is the
interface an agent reads changing. `datamk release`'s meaning ratchet keeps
hashing `cell.yaml`'s own words only.

No deprecation window: v3 is not served side by side. `datamk mesh emit`
reads both v1–3 and v4 documents.

### Fixed — SECURITY: a relative `principals:` path resolved against the process cwd

A profile's `principals:` was the one path field expanded but never rebased
against the cell directory — unlike `gcs.credentials`, `gcs.extension`, and
Snowflake `private_key_path`, which `config::load` has always rebased. A
relative path therefore resolved against wherever the process was started.

**Impact.** `datamk serve -f sub/cell.yaml` run from a parent directory
loaded the wrong token map, or none — and a cell that finds no principals
file falls back to `shareable`-only, so a role-gated cell could authorize a
caller its own file would have rejected. Serving a project made this
systematic rather than occasional: every mounted cell loaded whichever
cwd-relative file happened to exist, and each cell's own file was silently
ignored. The server started healthy and returned 200s, so nothing surfaced
the substitution.

Unaffected: any profile with an absolute `principals:`, which includes every
Kubernetes deployment (`/etc/datamk/principals.json`, checked by the deploy
pre-flight).

**Behavior change.** A relative `principals:` now resolves against the cell
directory. If you were relying on cwd-relative resolution — most likely by
running `datamk serve` from inside the cell directory, where the two agree —
nothing changes. If your token map lived somewhere else and worked by
accident, it will now fail to open at startup rather than authorize against
the wrong file. Make the path absolute, or move the file into the cell.

### Added — serve several cells from one process (`datamk.yaml`, ADR 0014)

A root `datamk.yaml` lists the cells one `datamk serve` process mounts behind
one port, each at `/<mount>/…`:

```yaml
datamk: 1
profile: prod
cells:
  - datamk-examples/weather
  - path: dplat-datamake/flight-spend
    profile: local
    mount: flights
    no_data: true
```

`datamk serve` with no `-f` uses `datamk.yaml` when it's in the current
directory, else `cell.yaml`; `-f` accepts either and dispatches on the file's
top-level key. `-p` overrides the project default and every per-cell
`profile:`. `--no-data` unions with per-cell `no_data:`. `--max-concurrency`
applies per mounted cell — a shared cap would let one cell's saturation shed
every other cell's liveness route.

Each cell keeps its own connection, catalog, poller, principals file, and
authorization policy. `GET /` lists only the mounts the caller's token can
reach (names only — discovery across cells stays `datamk mesh emit`'s job),
and every listed cell must open or the server does not start.

**Serving one cell is unchanged** — flat routes, no `servers` block, same
headers, and the same interface digest a cell has when mounted.

`datamk deploy` does not read this file; it still renders a single-cell
workload.

### Changed — BREAKING: request affordances in the context document are relative (`datamk_context: 3`)

`declared.include_request`, `declared.exports[].query.sample_request`, and
`observed.exports[].example_request` no longer carry a leading slash:

```diff
- "sample_request": "/orders_daily@2?limit=10"
+ "sample_request": "orders_daily@2?limit=10"
- "include_request": "/context?include=docs"
+ "include_request": "context?include=docs"
```

**Migrate:** resolve them against the document's own URL (RFC 3986) instead
of against the origin. A client that concatenated `origin + sample_request`
now needs `origin + "/" + sample_request` for a root-mounted cell — or, and
this is the point, any standard URL-resolution call, which is then correct
for a mounted cell too.

These strings live inside `declared`, which the interface digest hashes
whole. Root-absolute, they either point at the process root (a 404 in a
multi-cell server) or force the digest to depend on where a cell happens to
be served — putting deployment inside the contract. Relative, one string is
correct in both modes and the digest never sees the mount.

Every interface digest changes once as a result. `mesh emit` copies context
summaries from documents at version 3 (a version gate previously accepted
only 1 and 2, and would have silently dropped every copied field).

### Added — `datamk interface import` (issue #18)

Emit a ready-to-edit bound export block from a warehouse object's own live
types:

```
datamk interface import -p prod --bind gold_customer --as qfai_customer
```

`--bind` names a `sources:` entry (optional when the cell declares exactly
one); `--as` names the new export (default: the source's own name). Types
come from the same warehouse-native authority `verify` already uses for a
bound export (issue #9) — a column with no clean datamk type name is
emitted as `type: unmapped` with the real type named in a comment, never
dropped and never guessed. Prints the YAML block to stdout (pipeable
straight into `cell.yaml`) with everything else on stderr; `--write`
splices it directly into the file's `interface:` list instead — a
byte-range textual edit, never a full re-serialize, so every existing
comment in the file survives untouched. Refuses an existing export of the
same name unless `--force`.

**Deliberately never emits a description.** Copying warehouse prose into
`cell.yaml` is the exact rot ADR 0012 §3 warns about; types are safe to
copy because `verify` checks them against the warehouse every run; a copied
description has no such check and would go stale silently. The emitted
block carries `# description:` commented out as a prompt — the warehouse's
own column documentation already rides `observed.source_descriptions`
(issue #10), live, every time `datamk verify` runs. Correspondingly,
`contract: supported` no longer requires a locally authored description on
a bound export whose source already has warehouse-documented columns —
meaning available at the source satisfies the promotion gesture exactly as
well as meaning restated in `cell.yaml`, and forcing the restatement would
have made `interface import` re-introduce the rot it exists to avoid.

### Changed — BREAKING: `materialize: never` is retired; virtual cells become bindings (issue #6)

A transform declared `materialize: never` no longer parses into a working
cell. `datamk run`/`verify`/`config load` refuse it with a migration error
naming the offending export(s) and transform, explaining why in one clause,
and giving both exits — add `materialize: replace`/`declarative`/`incremental`
(needs no coordination with another team), or convert the export to a
binding (below) — with a best-effort hint when the transform is a pure
`SELECT * FROM <source>` that a straight `bind:` conversion would cover.

**Migrate:** replace

```yaml
transforms:
  - sql: sql/customer_pii.sql
    materialize: never
```

with a direct binding on the export:

```yaml
interface:
  - name: customer_pii
    version: 1.0.0
    bind: pii            # names a sources: entry directly; no SQL runs
sources:
  pii: { connection: crm, table: raw.customers }
```

`bind:` accepts a raw file source or a connection source with `table:` set
(not a `query:`-shaped connection, and not another cell's table — read
those through a materializing transform instead). Everything the old
mechanism produced — `query: null` in `/context`, the unmounted-route 404,
`status: verified_at_source` after a live `datamk verify` — is unchanged in
shape; only the `cell.yaml` syntax and what `datamk` is willing to run for
it changed. See `docs/adr/0012-cell-context-document.md`'s 2026-08-10
"`materialize: never` is retired" amendment for the full rationale.

### Changed — a cell with zero materializing transforms now refuses `run`

Previously a cell with no `transforms:` at all quietly built nothing and
succeeded. `datamk run` now refuses it outright (there is no snapshot to
commit) and points at `datamk verify`/`datamk context` instead — the same
refusal an all-bound cell already got, generalized to any cell that builds
no snapshot, empty or otherwise.

### Added — verify checks a bound export's type against the warehouse's own metadata (issue #9)

Where the connector has one (BigQuery today), a bound export's declared
column type is checked against the warehouse's own native type
(`INFORMATION_SCHEMA.COLUMNS.data_type`), not DuckDB's `DESCRIBE` of the
bound session view. This is the fix for a wide BigQuery `NUMERIC`/
`BIGNUMERIC` column, which DuckDB can only render as `VARCHAR` once
attached — previously forcing `verify` to reject a correctly declared
`decimal`. Postgres and Snowflake run no metadata job (unchanged) and keep
DuckDB's `DESCRIBE` as the only authority for their bound exports.

### Added — `observed.source_descriptions` (issue #10)

The same live-verify pass surfaces upstream column descriptions — a
machine-observed fact, source name -> column name -> description — under
`observed.source_descriptions` on both `datamk context` and the hosted
`/context`. Populated only where the connector has a metadata job
(BigQuery today); never appears in `declared` (author-reviewed prose) and
never feeds `docs:`'s release-time digest, so an upstream comment edit in
someone else's warehouse can never move a release gate. Persisted to
`.cell/source_descriptions.json` (sibling of `.cell/source_check.json`,
same digest-and-profile-gated freshness discipline) and shipped in the
deploy artifact so a hosted Server can serve it without its own warehouse
round trip.

### Fixed — `served_here` no longer disagrees between the two doors

`datamk context` and the hosted `/context` now compute `data.served_here`
from the same predicate, closing a gap where the two could disagree about
whether a cell's data routes were actually mounted.

### Changed — the `query` block is unconditional interface grammar

Every export's `query` block (filters, limits, `sample_request`) is no
longer omitted under `--no-data` — it describes the query grammar, an
interface fact, not whether this particular server currently mounts the
route (`data.served_here` already carries that). It still nulls for a
bound export, unchanged, since that export has no query affordance ever,
regardless of any flag. See ADR 0012's 2026-08-10 "the `query` block is
unconditional" amendment.

### Added — hosted `/context` surfaces `source_check`

The hosted `/context` now reads `.cell/source_check.json` at startup
(digest- and profile-gated, same as the portable `datamk context` door),
closing a gap where an all-bound cell's hosted `/context` was permanently
`status: draft` even though a live `datamk verify` had already earned it
`verified_at_source`.

`/context`'s `ETag` now folds in a short hash over these startup-fixed
observed inputs (`source_check`/`source_descriptions`), not just the
interface digest. Without this, a caching client's `If-None-Match` could
304 straight past a rollout that first shipped `status: verified_at_source`
or a new `observed.source_descriptions` entry — `cell.yaml` itself is
unchanged across that rollout, so the interface digest alone never moved.
A cell with neither observed input keeps its exact byte-identical `ETag`.

### Changed — a bound (virtual) cell is deployable (issue #11)

`datamk deploy`'s pre-flight no longer refuses an all-bound cell outright —
only where the target genuinely has nothing to run (no long-lived Server
capability at all). On Kubernetes, the one-shot init Job that normally
initializes the catalog before the Server is applied is not rendered or
applied at all for an all-bound cell (there is nothing to build — a
rendered Job would only ever run `datamk run`'s own refusal inside the
pod and fail the whole deploy); a `schedule:` set together with an
all-bound cell is separately refused (there is no Builder to run; the
CronJob would crash-loop every scheduled tick), and no Builder workload is
ever reported for such a cell. The open-endpoint refusal now names the
document itself as the payload for a bound cell — declared columns, grain,
and prose (which can themselves name upstream fields) is exactly what an
anonymous caller gets, worth the same review as any other cell's open
endpoint, not a lesser one.

### Fixed — `declared.exports[].route`'s documentation

`route` was documented as unconditionally "the serving route key." For a
bound export it never was one — `/openapi.json`'s paths already excluded
it. `route` stays present for every export (it doubles as the export's
docs `target`), but its documented meaning is now precise: check `query`
(`null` for exactly the bound exports) before building a URL from `route`,
never `route`'s mere presence.
