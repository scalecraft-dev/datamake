# Changelog

All notable user-visible changes to `datamk` are recorded here. Format
loosely follows [Keep a Changelog](https://keepachangelog.com/); dates are
`YYYY-MM-DD`.

## [Unreleased]

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
