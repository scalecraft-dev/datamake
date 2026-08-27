# Discovered cells: a SQLMesh project's deployed models as an interface

A cell normally *authors* its interface: you write `interface:` and
`datamk verify` proves it. A **discovered** cell reads its interface from a
modeling tool's deployed state instead — today SQLMesh (ADR 0016) — so
schema, grain and descriptions keep one home and are never hand-copied.
Point the cell at the project once; `datamk sync` does the rest.

What you get, per selected model: a bound export (typed columns, the
model's own description and column descriptions, its grain), lineage inside
the cell, and the tool's deployment facts (kind, cron, owner, tags, the
deployed snapshot fingerprint, loaded intervals). What you don't get: rows
over HTTP. A discovered cell serves `/context` and `/openapi.json`; the rows
stay in the warehouse, and every export's `binding.object` says where.

## Point at the project

```bash
datamk init gold --from sqlmesh
```

```yaml
# cell.yaml
cell: gold
description: The invoicing marts.

discover:
  from: sqlmesh
  environment: prod            # the deployed environment — never a dev env
  state: sqlmesh_state         # profiles/<p>.yaml connections.sqlmesh_state
  warehouse: warehouse         # profiles/<p>.yaml connections.warehouse
  select:                      # REQUIRED: at least one of tags/schemas/models
    schemas: [invoice]         #   OR within a key, AND across keys
    tags: [invoicing_step]
    # models: [margins.flight_margin]
    # kinds: [FULL, VIEW]      # default: every kind except EXTERNAL and SEED
  exclude:
    models: [invoice.scratch_x]
  overrides:                   # refine a discovered model; never invent one
    - model: invoice.flight_spend
      as: flight_spend
      version: 1.2.0           # authored semver — required to promote
      contract: supported
      grain: [month, campaign_group_id, flight_id]

access:
  shareable: true
```

`discover:` and any of `sources:`/`transforms:`/`interface:` together is
a parse error: a discovered cell computes nothing and authors no export
list. `select` has no "everything" default — an interface is an explicit
export list, and a 2,000-model DAG is not one. The shape that shipped is
**one cell per modeling project**, its `select` naming the models that
project publishes; a project whose models serve unrelated consumers can be
split into several cells, composed in `datamk.yaml` like any others.

The profile names two connections — the tool's **state store** and the
**warehouse** where the deployed objects live. A profile is a closed
shape: an unknown key (a typo, or a field a release removed) is a parse
error, never silently ignored.

```yaml
# profiles/prod.yaml
storage: gs://acme-cells/gold
channels:
  - "Rows live in BigQuery dw-main-silver; see each export's binding.object."
connections:
  sqlmesh_state:              # SQLMesh's state_connection
    type: postgres
    host: ${SQLMESH_STATE_HOST}
    database: sqlmesh
    user: ${SQLMESH_STATE_USER}
    password: ${SQLMESH_STATE_PASSWORD}
  warehouse:
    type: bigquery
    project: dw-main-silver
```

`storage:` is required by the profile shape but never read or written for a
discovered cell — no object-store credentials are needed for `sync`,
`verify`, `context` or `serve`.

A modeling project usually spans catalogs (`dw-main-bronze`/`silver`/
`gold` as three BigQuery projects). One BigQuery `warehouse` connection
reads every selected model from the model's *own* project, billing the
metadata jobs to the connection's `billing_project` (else its `project`) —
so that identity needs `bigquery.jobs.create` there and metadata read on
each project the selection touches. A Postgres or DuckDB connection is one
database; models in another catalog are reported as unreachable.

On a laptop a SQLMesh project's state store *and* its objects are one
DuckDB file, so both connections are `type: duckdb` pointing at it. A state
store held in Postgres or a DuckDB file is supported; BigQuery/Snowflake
state stores are not yet. The warehouse can be BigQuery, Postgres, or a
DuckDB file (Snowflake: not yet).

### Cloud SQL state stores

datamk speaks libpq (DuckDB's Postgres scanner), not the Cloud SQL Python
connector. The whole recipe:

```bash
cloud-sql-proxy --auto-iam-authn <project>:<region>:<instance> --port 5432 &
export SQLMESH_STATE_USER=<your IAM principal>          # a service account's IAM user name
export SQLMESH_STATE_PASSWORD=$(gcloud auth print-access-token)   # drops .gserviceaccount.com
datamk sync -f cell.yaml -p prod
```

with `host: 127.0.0.1`, `port: "5432"`, `sslmode: disable` (the proxy
terminates TLS locally) on the state connection. `password:` must resolve
to a non-empty string even though the proxy ignores it — a placeholder is
fine; empty is rejected at profile load. In a job, the proxy is a
sidecar and the token comes from the pod's service account. `sync` runs and
exits; the token's one-hour life is never a problem.

## Running in a container

- **Memory.** `verify` stages every bound object through DuckDB; DuckDB's
  default memory limit is a fraction of *host* RAM, which inside a cgroup is
  the wrong number — the kernel kills the process before DuckDB would
  spill. datamk reads the container's cgroup limit and defaults DuckDB to
  75% of it (logged at start-up); set `DATAMK_MEMORY_LIMIT` to choose
  explicitly, and keep the pod's limit above what a staged read needs.
- **Extensions.** A fresh binary downloads ~200 MB of DuckDB extensions
  (ducklake, httpfs, json, postgres, sqlite, community bigquery) from the
  registry at first use into `$HOME/.duckdb`, and needs an exec-capable
  filesystem to load them. The release image bakes them
  (`datamk debug install-extensions` at build); if you build your own,
  do the same.
- **Storage.** With an all-bound (discovered) cell, `serve` polls nothing
  and never touches `storage:` at steady state.
- **verify cost.** Every `verify` stages every bound object — on the live
  ape cell, 1.8M rows and ~450 MB billed per run. Don't run it at every pod
  start on a cell with big tables; run it on a schedule (a CronJob calling
  `datamk verify -p prod`, shipping the record with the next deploy).

## Sync

```bash
datamk sync -f cell.yaml -p prod
```

```
Discovered 1969 models from sqlmesh environment 'prod' (plan ea22e379…, finalized 2026-08-24T22:51:46Z); 41 selected
  41 exports · 1928 excluded by discover.select · column descriptions: 118 from sqlmesh, 203 from the warehouse, 64 none
Wrote .cell/deployed_catalog.json
Next:
  datamk verify  -p prod    # live-check types against the warehouse
  datamk context -p prod    # the document agents read
  datamk serve   -p prod    # /context + /openapi.json; rows stay in the warehouse
```

`sync` is the only step that needs credentials. Each BigQuery
`INFORMATION_SCHEMA` read is one job per (project, dataset) and bills
BigQuery's 10 MiB-per-table minimum. It reads the state store
(read-only `SELECT`s over `_versions`, `_environments`, `_snapshots`,
`_intervals` — no Python, no SQLMesh install) and the warehouse's
`INFORMATION_SCHEMA` (batched per schema), and writes one file. Everything
downstream — `verify`, `context`, `serve`, `deploy` — reads that file with
no credentials at all. It ships inside the deploy artifact like
`published.json` does, so a deployed Server carries the interface it was
synced with.

Where each fact comes from, in order:

| fact | first | then | then |
|---|---|---|---|
| column names, types | the model's `columns (...)` | the warehouse | — |
| column description | the model's `column_descriptions` | the model's inline `--` comments (SQLMesh's own rule) | the warehouse's registered comment |
| model description | the model's `description` | the warehouse's object comment | — |
| grain | the model's `grains` | `overrides[].grain` | empty |

The first two description sources are both *SQLMesh's* and land with
`from.description: "sqlmesh"`; the warehouse copy (what `register_comments`
wrote) is the fallback, `"warehouse"`. An override is `"cell.yaml"` and
wins. One home per fact — the losers are not emitted.

A model whose columns can't be resolved (undeclared, and no warehouse
object — usually not yet planned, or a missing metadata grant) fails the
sync with the three fixes named; `on_unresolvable: exclude` drops it and
lists it in the document's `notes[]` instead.

An override names a model. If that model is no longer among the selected
ones — renamed, moved to another schema, removed, or excluded by `select`
— `sync` warns, naming the model and the export and docs page that will not
appear; `on_missing_override: fail` makes it refuse instead. It never
vanishes silently.

A newly deployed model that lands in a selected schema is picked up by the
next `sync`; if the serving identity lacks a grant on what it reads,
`verify` fails on it. A grant precedes every newly served model; `exclude`
is the escape until it lands.

**Deployed reality only.** `sync` reads the environment row a plan
promoted, joins snapshots on `(name, identifier)` — never on name alone —
and refuses an environment that isn't finalized (an apply in flight; exit
code 75 so a scheduler can retry). A dev environment's edits never appear.
The state schema version is pinned (`100`); a store written by a newer
SQLMesh fails loud rather than being misread.

## What the other verbs do

| verb | on a discovered cell |
|---|---|
| `run` | refuses — nothing to build; use `sync` |
| `verify` | live-checks every export's types and grain against the warehouse, each read from the model's own project through the jobs API; earns `status: verified_at_source`. The grain check scans the table — mind the bill on large models |
| `context` | the document, with `discovered_from` and per-export `deployed`; a draft with a note if the record is missing, or was synced from a different `cell.yaml` or profile |
| `serve` | serves `/context` + `/openapi.json`; **refuses to start** without a fresh record (an empty interface with a valid ETag would read as "no exports") |
| `release` | pins `supported` exports; the meaning ratchet hashes authored prose only, so an upstream description edit moves the ETag, never the release gate |
| `status` | the plan, sync time, and export count |
| `attach`, `rollback` | refuse — datamk owns no rows or lineage here |
| `deploy` | renders a Server only — no Builder CronJob, no init Job (nothing to build); the record ships in the artifact |

## Versions and contracts

SQLMesh has fingerprints, not semver. Discovered exports are `1.0.0`,
`contract: experimental`, route `name@1` — unpinnable, and the document
says so. To promote one, write an override with an authored `version` and
`contract: supported`. From then on `sync` **refuses to overwrite** the
record when that model's data definition changed upstream and the version
did not — the upstream moved a contract datamk promised; bump the version
(MAJOR if the meaning changed) or exclude the model.

## The document

```json
{
  "cell": "gold",
  "status": "verified_at_source",
  "discovered_from": { "tool": "sqlmesh", "environment": "prod",
                       "plan_id": "ea22e379…", "finalized_at": "2026-08-24T22:51:46Z",
                       "synced_at": "2026-08-25T04:02:11Z", "evidence": "environment_row" },
  "exports": [{
    "name": "flight_spend", "version": "1.2.0", "route": "flight_spend@1",
    "contract": "supported",
    "description": "Spend by flight_id and month for invoicing and UI reporting",
    "grain": ["month", "campaign_group_id", "flight_id"],
    "from": { "description": "sqlmesh", "grain": "cell.yaml" },
    "schema": {
      "month":          { "type": "DATE",    "from": { "type": "warehouse" } },
      "invoice_amount": { "type": "NUMERIC", "description": "…",
                          "from": { "type": "warehouse", "description": "sqlmesh" } }
    },
    "binding": { "source": "flight_spend", "object": "dw-main-silver.invoice.flight_spend",
                 "connection": "warehouse" },
    "depends_on": ["public_advertisers@1", "ui_ui_flights@1"],
    "depends_on_unselected": 2,
    "deployed": { "at": "2026-08-25T04:02:11Z", "model": "dw-main-silver.invoice.flight_spend",
                  "kind": "INCREMENTAL_BY_TIME_RANGE", "cron": "@hourly", "owner": "ber",
                  "tags": ["invoicing_step"], "fingerprint": "3237974788",
                  "intervals": { "start": "2000-01-01T00:00:00Z", "end": "2026-08-24T18:00:00Z" } }
  }]
}
```

There is no `query` key: a bound export has no data route (a hand-authored
one emits `query: null` for the same fact; the field is simply absent
here). `binding.object` is the model's own project-qualified object on
BigQuery, the bare `schema.table` on a single-database warehouse.

`deployed` and `discovered_from` are measurements (they carry `at`/
`synced_at`) and sit outside the interface digest: an upstream tag or cron
edit never moves the ETag. `description` and `schema` are the interface, so
an upstream *meaning* change does — correctly. `depends_on` names selected
parents by route key; unselected ones are a count, never names.

## Docs pages

A discovered export's meaning fields live upstream. The one authored thing
per export that has no home there is the long-form consumer page — how to
join it, what not to use it for — and that is `docs:` on an override,
exactly as on a hand-authored export (ADR 0013): one relative path, under
the cell directory, 64 KiB per page, 256 KiB per cell.

```
cells/ape/
├── cell.yaml
├── docs/
│   ├── cell.md                 # cell-level: what this set of models is for
│   └── paid_media_daily.md     # one page per export that needs one
└── .cell/deployed_catalog.json # written by sync, never by hand
```

```yaml
docs: docs/cell.md
discover:
  overrides:
    - model: ape_mktg.paid_media_daily
      as: paid_media_daily
      docs: docs/paid_media_daily.md      # an override may carry docs alone
```

The page lands in `docs[]` under the export's route key
(`paid_media_daily@1`), with `content` under `?include=docs`; adding or
renaming a page moves the digest, editing its prose does not, and `release`
folds it into the meaning ratchet. A page that breaks the rules (outside
the cell directory, absolute, over the cap) fails `sync` — the step that
has credentials — as well as `context` and `serve`.

## Deploying, and when the record refreshes

A discovered cell's interface changes when the SQLMesh project's `prod`
changes, so the pipeline is the contract:

```
sqlmesh plan prod  →  datamk sync -p prod  →  datamk deploy -p prod
```

The record `sync` wrote ships inside the deploy artifact; `deploy` renders a
Server only — no Builder CronJob and no init Job, since there is nothing to
build. There is deliberately **no clock**: the record is exactly as current
as the last deploy by construction, and `discovered_from.plan_id` /
`synced_at` say which plan a reader is looking at. What does invalidate it:
a `cell.yaml` edit since the sync, or a profile other than the one it was
synced under — then `serve` refuses to start and `context` says so in
`notes[]`.

## Checking the inline-comment extractor against your project

datamk reproduces SQLMesh's rule for inline column comments without running
SQLMesh. To prove it on your own models, dump SQLMesh's answer next to the
project and let datamk diff it — the SQL never leaves your machine:

```python
# in the SQLMesh project — parses the models only; the state store is never touched
import json
from sqlmesh import Context
from sqlmesh.core.config import DuckDBConnectionConfig
import config as cfgmod          # your config.py; a config.yaml project can use Context(paths=".")
cfg = cfgmod.config
for gw in cfg.gateways.values():
    gw.state_connection = DuckDBConnectionConfig(database=":memory:")
c = Context(paths=".", config=cfg)
cases = {}
for m in c.models.values():
    q = getattr(m, "query", None)   # seeds have none
    if m.kind.name == "EXTERNAL" or m.column_descriptions_ is not None or q is None:
        continue
    cases[m.name] = {"sql": json.loads(m.json())["query"]["sql"],   # the text the state store holds
                     "dialect": m.dialect, "expected": m.column_descriptions}
json.dump(cases, open("comments_check.json", "w"))
```

```bash
datamk debug sqlmesh-comments comments_check.json   # exits non-zero on any mismatch
```

Run against a 1,969-model production project (977 non-external models
without declared descriptions, 68 with inline ones) on 2026-08-25: 0
mismatches, on both the stored model text and sqlglot's re-serialization
of it.
