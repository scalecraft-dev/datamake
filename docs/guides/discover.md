# Discovered cells (SQLMesh)

A discovered cell reads its interface from a SQLMesh project's deployed
state instead of an authored `interface:`. It serves `/context` and
`/openapi.json` only; rows stay in the warehouse, and each export's
`binding.object` says where.

```bash
datamk init gold --from sqlmesh
datamk sync -f cell.yaml -p prod
```

```yaml
# cell.yaml
cell: gold
discover:
  from: sqlmesh
  environment: prod
  state: sqlmesh_state        # profiles/<p>.yaml connections.<name>
  warehouse: warehouse        # profiles/<p>.yaml connections.<name>
  select:
    schemas: [invoice]
    tags: [invoicing_step]
  exclude:
    models: [invoice.scratch_x]
  overrides:
    - model: invoice.flight_spend
      as: flight_spend
      version: 1.2.0
      contract: supported
      grain: [month, campaign_group_id, flight_id]
      docs: docs/flight_spend.md
access:
  shareable: true
```

| Key | Default | Meaning |
|---|---|---|
| `from` | required | `sqlmesh` |
| `environment` | `prod` | SQLMesh environment to read; must be finalized |
| `state` | required | Profile connection for the state store: Postgres or a DuckDB file |
| `state_schema` | `sqlmesh` | Schema holding the state tables |
| `warehouse` | required | Profile connection where model objects live: BigQuery, Postgres, or DuckDB |
| `select.tags` / `.schemas` / `.models` | at least one required | OR within a key, AND across keys |
| `select.kinds` | all except `EXTERNAL`, `SEED` | Model kinds to include |
| `exclude.models` / `.schemas` | none | Drop from the selection |
| `on_unresolvable` | `fail` | `exclude` drops a model with no resolvable columns and lists it in `notes[]` |
| `on_missing_override` | `warn` | `fail` refuses when an override names an unselected model |
| `overrides[].model` | required | `schema.table` |
| `overrides[].as` | `<schema>_<table>` | Export name |
| `overrides[].version` | `1.0.0` | Authored semver; required for `contract: supported` |
| `overrides[].contract` | `experimental` | |
| `overrides[].grain` | model `grains` | |
| `overrides[].description` | model description | Replaces the tool's text |
| `overrides[].visibility` | | `private` hides the export |
| `overrides[].docs` | | Relative path under the cell; 64 KiB per page, 256 KiB per cell |

Rules: `discover:` cannot coexist with `sources:`/`transforms:`/`interface:`.
Unknown profile keys are parse errors. `storage:` is required by the profile
shape but never used. Cell-level `docs:` and `definitions:` work as on any
cell.

Fact precedence: column types from the model's `columns`, then the
warehouse. Column descriptions from `column_descriptions`, then inline `--`
comments, then the warehouse comment. Grain from `grains`, then the
override. Each value carries its origin in `from`.

## Sync

`datamk sync -f cell.yaml -p prod` reads the state store (`_versions`,
`_environments`, `_snapshots`, `_intervals`, read-only) and the warehouse
`INFORMATION_SCHEMA`, then writes `.cell/deployed_catalog.json`. Every other
verb reads that file and needs no credentials. Re-run after every
`sqlmesh plan prod`. The pipeline is `sqlmesh plan prod` → `datamk sync` →
`datamk deploy`.

- Refuses an unfinalized environment with exit code 75 (retryable). Refuses
  a state schema version other than 100.
- Refuses to overwrite the record when a `supported` model's definition
  changed upstream and its `version` did not. Bump the version or exclude
  the model.
- BigQuery: one job per (project, dataset), each billed at the 10 MiB
  minimum. The identity needs `bigquery.jobs.create` on `billing_project`
  and metadata read on every project selected.

Cloud SQL state store: run `cloud-sql-proxy --auto-iam-authn <project>:<region>:<instance> --port 5432`,
set `host: 127.0.0.1`, `sslmode: disable`, `user` to the IAM principal, and
`password` to `$(gcloud auth print-access-token)`. The password must be
non-empty even though the proxy ignores it.

Containers: set `DATAMK_MEMORY_LIMIT` (default is 75% of the cgroup limit).
Bake DuckDB extensions with `datamk debug install-extensions`. Run `verify`
on a schedule, not at pod start; it stages every bound object.

## What the other verbs do

| Verb | On a discovered cell |
|---|---|
| `run`, `attach`, `rollback` | Refuse |
| `verify` | Live-checks types and grain against the warehouse; sets `status: verified_at_source`. Grain check scans the table |
| `context` | Emits the document with `discovered_from` and per-export `deployed`; a draft with a note if the record is missing or stale |
| `serve` | Serves `/context` and `/openapi.json`; refuses to start without a fresh record |
| `release` | Pins `supported` exports; the meaning ratchet hashes authored prose only |
| `status` | Plan id, sync time, export count |
| `deploy` | Renders a Server only; the record ships in the artifact |

## Gotchas

- Exports default to `1.0.0`, `experimental`, route `name@1`. Promotion
  requires an override with `version` and `contract: supported`.
- The record is stale after a `cell.yaml` edit or under a different profile.
  `serve` refuses; `context` says so in `notes[]`.
- `deployed` and `discovered_from` sit outside the interface digest.
  `description` and `schema` are inside it. A docs page rename moves the
  digest; a prose edit does not.
- `depends_on` names selected parents by route; `depends_on_unselected` is
  a count.
- A dev environment's edits never appear. Snapshots join on
  `(name, identifier)`.
- Verify the inline-comment extractor against your project by dumping
  SQLMesh's `column_descriptions` per model to JSON and running
  `datamk debug sqlmesh-comments <file>` (non-zero exit on mismatch).

Design rationale: [ADR 0016](../adr/0016-discovered-interfaces-sqlmesh.md).
