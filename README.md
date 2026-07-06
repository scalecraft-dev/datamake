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
```

Transforms filter through the view with full pushdown — write plain SQL against
`crm_accounts` and DuckDB pushes projections/filters into the warehouse scanner.

### Incremental loading

A `connection` source can declare a cursor so the Builder reads only rows
past a persisted watermark instead of re-scanning the whole table every run:

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

## Install / build

Datamake is a single Rust binary. Building it requires the Rust toolchain
(`rustup`); the first build compiles a bundled DuckDB and is slow, after which
builds are incremental.

```bash
cargo build --release
```

---

## Licensing

This project is freely available under the Apache License 2.0. Datamake is free and will always be free. There are no gated features, or paid subscription plans.
