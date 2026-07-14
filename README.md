# Datamake

**Build data products you can ship, version, and trust.**

Datamake (`datamk`) lets you package a transform, the data it produces, and the
promise of what that data looks like into one self-contained, deployable unit
called a **cell**. Run it anywhere, serve it over HTTP, and evolve it without
breaking the people who depend on it.

---

## What's a composable data product (CDP)?

Why should I care about CDPs if I already have the typical 
data stack of warehouse + dbt/sqlmesh + airflow/dagster? Great question, start here...

[WTF is a Composable Data Product?](./docs/concepts/composable-data-products.md)

---

## What is a cell?

A cell is a small project directory. It represents the basic unit of function in Datamake, and follows
a gitops SDLC.

```bash
# Create a cell called `orders`
datamk init orders && cd orders
```

```tree
orders/
  cell.yaml        # the contract: sources, transforms, interface, access [tracked]
  sql/*.sql        # private logic; runs in order → one atomic snapshot   [tracked]
  profiles/
    local.yaml     # laptop bindings (./.cell paths, no secrets)          [tracked]
    prod.yaml      # storage + S3 creds (no catalog — ADR 0004)           [gitignored]
  deploy/
    prod.yaml      # where/how the workloads run in prod                  [tracked]
```

`cell.yaml` carries **no environment config**. The same cell runs on your
laptop and in prod unchanged; only `--profile` selects different bindings.

**1. Declare the contract** (`cell.yaml`). Transforms are private; only what's
listed under `interface` is exposed:

```yaml
cell: orders
sources:                        # external inputs, bound by name
  raw_orders: ${ORDERS_PATH:-s3://acme-lake/orders/*.parquet}
transforms:                     # run in order, atomically → one snapshot
  - sql/stg_orders.sql
  - sql/orders_daily.sql
interface:                      # the public surface
  - name: orders_daily
    version: 2.1.0              # semver; route keys on MAJOR → /orders_daily@2
    grain: [order_date, region] # filterable params, uniqueness-checked
    schema: { order_date: date, region: string, revenue: decimal }
    contract: experimental      # promote to `supported` via PR
access:
  shareable: true               # default-deny until you say otherwise
```

**2. Build it.** `datamk run -f cell.yaml` executes the transforms, commits one
atomic snapshot to an embedded **DuckLake** (zero external services locally),
and auto-verifies the output against the interface — the contract can't
silently drift from reality.

**3. Serve it.** `datamk serve -f cell.yaml` exposes the interface as REST +
OpenAPI: `GET /orders_daily@2?region=us-east`, `GET /openapi.json`.

**4. Release it.** Promote via PR (`contract: supported`), then `datamk release`
pins the current snapshot. That frozen snapshot is what other cells — and other
teams — build on.

**5. Deploy it.** `datamk deploy -p prod` runs the cell's workloads on an
orchestrator — see [Deploying](#deploying).

---

## Sources

A cell's external inputs, bound by name as session-local views before
transforms run. Three kinds:

```yaml
sources:
  raw_orders: s3://acme-lake/orders/*.parquet   # a raw path/URI (Parquet/CSV/JSON, globs ok)
  customers:                                    # another cell's versioned table
    cell: customers
    table: dim_customers
  crm_accounts:                                 # a warehouse table via a named connection
    connection: crm                             # -> the profile's `connections.crm`
    table: sales.accounts
```

*Which table* is contract (`cell.yaml`); *which project and credentials* is
environment. The profile supplies the connection, so the same cell reads a
sandbox project in dev and the real one in prod:

```yaml
# profiles/prod.yaml
connections:
  crm:
    type: bigquery                # the only connector today; more to come
    project: acme-prod-crm
    # credentials: /etc/datamk/bq-key.json   # service-account key; omit to use ADC
    # staging_uri: gs://acme-bq-staging/datamk-scratch  # oversized view reads only; see below
```

Transforms filter through the view with full pushdown — write plain SQL against
`crm_accounts` and DuckDB pushes projections/filters into the warehouse scanner.
View-backed connection sources (a BigQuery view, materialized view, or
external table) are auto-detected and read via the BigQuery jobs API instead
— no DuckDB pushdown; the full view materializes every run unless
`incremental:` bakes the watermark predicate into the issued query. If a
jobs-API result exceeds BigQuery's ~10GB anonymous-result ceiling, set
`staging_uri:` (a scratch object-store prefix) to escalate to `EXPORT DATA`
instead of failing — needs `storage.objects.create`/`delete` on that prefix
for the warehouse identity, in addition to `bigquery.jobs.create`.

### `query:` sources — server-side shaping

A `connection` source may declare **`query:`** — warehouse-dialect SQL
executed server-side — instead of `table:`. It's the escape hatch for a
source scan-with-pushdown can't express, most often a value-shaping
aggregation that shrinks what crosses the wire (and what's billed to be
*returned*, not scanned):

```yaml
sources:
  raw_spend_hourly:
    connection: dw_silver
    query: |
      SELECT advertiser_id, parent_campaign_group_id, campaign_group_id,
             hour, CAST(SUM(total_spend) AS NUMERIC) AS total_spend,
             CAST(SUM(media_cost) AS NUMERIC) AS media_cost
      FROM `${connection.project}.summarydata.campaign_group_spend_by_minute`
      GROUP BY 1, 2, 3, 4
```

`query:` and `table:` are exactly-one-of. `query:` is jobs-routed by
construction — same staging, same §3a ceiling escalation, same narration as
a view — and never composes with `incremental:` (v1; a `query:` source
re-reads in full every run). Before the real read, a free `dry_run := true`
preflight fails loud on a clearly-malformed query and narrates the exact
`total_bytes_processed` the real read will scan; after staging, the engine
DESCRIBEs the temp table and logs its column types. Four things worth
stating plainly:

- **Never hardcode a project in the query — write `${connection.project}`.**
  This is a reserved, engine-owned binding (env-var names can't contain
  `.`, so there's no collision with `${VAR}` expansion): the engine
  substitutes the connection's resolved `project` at resolve time,
  fully-qualifying `` `${connection.project}.dataset.table` `` the same way
  `qualify()` fully-qualifies a `table:` reference. This isn't just style —
  an unqualified `dataset.table` in a BigQuery job resolves against
  whichever project the job *runs in*, which under split billing
  (`billing_project` set and different from `project`) is the *billing*
  project, not the storage project, and fails loud with `Not found:
  Dataset <billing>:<dataset>`. Any other `${connection.*}` name is a
  resolve-time error naming the one supported binding.
- **A `query:` source narrows portability** to that warehouse family —
  GoogleSQL in `cell.yaml` isn't dialect-portable the way `table:` is.
  Cells that stay on `table:` keep the full same-cell-everywhere guarantee.
- **BigQuery numerics beyond DuckDB's range degrade silently to
  `VARCHAR`.** `BIGNUMERIC` (and full-range `NUMERIC` aggregates, e.g.
  `SUM(BIGNUMERIC)`) exceed DuckDB's `DECIMAL(38,·)`; the extension maps
  such columns to `VARCHAR` with no warning, and the engine cannot detect
  this authoritatively (there's no extension-exposed result schema for an
  arbitrary query). `CAST(… AS NUMERIC)` in the query body is the fix — and
  an **author correctness assertion** (it can overflow or round), not an
  engine check, so it's covered by the gated test below, not by anything
  automatic.
- **A value-shaping `query:` source needs a gated warehouse correctness
  test.** `verify` and `--verify-replay` both run downstream of the
  aggregation — a wrong `GROUP BY` produces wrong-but-schema-valid numbers
  neither check can catch. Assert the shaped read against a known fixture
  (see `tests/bigquery_query_correctness.rs`, gated on
  `DATAMK_TEST_BQ_PROJECT`).

### Incremental loading

A `connection` source can declare a cursor so the Builder reads only rows
past a persisted watermark instead of re-scanning the whole table every run.
Incremental works over views too (the predicate is baked into the jobs-API
query) and is the only way to avoid a full read per run for a view — though
bytes *billed* still depend on the view's own SQL (a CDC-dedup view may scan
its full base table regardless of the predicate), even though bytes
*returned* scale with the delta by construction:

```yaml
sources:
  events:
    connection: crm
    table: analytics.events
    incremental:
      cursor: updated_at   # a monotonic column; its max is the new watermark
```

Delivery is **at-least-once** — a transform reading an incremental source
must be replay-safe (an anti-join or `MERGE`, never `CREATE OR REPLACE`). See
[Incremental source loading](docs/guides/incremental.md) for the full guide,
`--full-refresh`/`--verify-replay`, and the edge cases.

---

## The CLI

| Command | Does |
|---|---|
| `datamk init <name>` | Scaffold a new cell. |
| `datamk run` | Execute the transforms, commit a snapshot, auto-verify. |
| `datamk verify` | Machine-check actual output against the declared interface. |
| `datamk release` | Pin the current snapshot as the supported contract. |
| `datamk serve` | Serve the interface as REST + OpenAPI. |
| `datamk deploy` | Run the cell as managed workloads on an orchestrator. |
| `datamk attach` | Print SQL that attaches the cell's catalog in DuckDB, read-only. |

`datamk attach` prints a stateless, portable recipe — runnable on any host with
credentials. The one exception is a native-GCS-extension profile
(`gcs.extension`, for orgs that forbid HMAC keys): DuckDB's extension cannot
attach a *remote* catalog file, so attach refuses by default and
`--download` opts into materializing the pinned execution under
`<cell>/.cell/attach/` (machine-local, reused on re-runs since artifacts are
immutable; delete the directory to reclaim space).

### Logs

`run`, `release`, `rollback`, and `deploy` — the commands that change
something — each write one plain-text log per invocation to
`<cell>/.cell/logs/datamk_<command>_<UTC-timestamp>.log` (`--log-dir`/
`DATAMK_LOG_DIR` to redirect; `--log-keep`, default 20, prunes older ones at
startup). `verify`/`status`/`init`/`attach` don't — `status` in particular
often runs in a watch loop, and retention is not a license to generate spray.
`RUST_LOG` governs both the console and the file, with one exception: the file
always pins `aws_config=warn` (credential-chain narration, which can include
access key ids at `info`) regardless of what `RUST_LOG` asks for — a human
debugging credentials locally can still raise it on their own ephemeral
terminal. Set `DATAMK_LOG=off` to disable file logging entirely (read-only or
ephemeral filesystems; the deployed image sets this by default — pod stderr
is the log pipeline in-cluster).

---

## Deploying

A cell has two production workloads: the **Builder** (`datamk run`, on a
schedule) and the **Server** (`datamk serve`, long-lived). `datamk deploy` runs
both on an orchestrator, driven by a tracked, secret-free `deploy/<profile>.yaml`
overlay next to your cell:

```bash
datamk deploy -f cell.yaml -p prod --dry-run   # render + review the manifests
datamk deploy -f cell.yaml -p prod             # apply
```

### Deployment Targets

- **[Kubernetes deployment guide](docs/guides/kubernetes.md)**

---

## Install

Datamake is a single binary. The installer grabs the latest release for your
platform (macOS Apple Silicon; Linux x86_64/arm64, glibc 2.28+), verifies its
checksum, and installs to `~/.local/bin`:

```bash
curl -fsSL https://raw.githubusercontent.com/scalecraft-dev/datamake/main/install.sh | sh
```

On Windows, run the same one-liner inside WSL2. On anything else (Intel Mac,
Alpine/musl), build from source with the Rust toolchain (`rustup`) — the
first build compiles a bundled DuckDB and is slow:

```bash
cargo install --git https://github.com/scalecraft-dev/datamake datamk
```

---

## Licensing

This project is freely available under the Apache License 2.0. Datamake is free and will always be free. There are no gated features, or paid subscription plans.
