# Datamake

**Build data products you can ship, version, and trust.**

Datamake (`datamk`) lets you package a transform, the data it produces, and the
promise of what that data looks like into one self-contained, deployable unit
called a **cell**. Run it anywhere, serve it over HTTP, and evolve it without
breaking the people who depend on it.

---

## What's a composable data product?

A *data pipeline* moves and transforms data. It's plumbing, valuable, but it
has no edges. Where does it start? Where does it end? What does it promise? Who
is allowed to use it? Usually those answers live in someone's head, a wiki, and
three Slack threads.

A **composable data product** draws the edges. It bundles four things that
normally drift apart:

| | Pipeline | Composable data product (a *cell*) |
|---|---|---|
| **Logic** | SQL/code, somewhere | versioned transforms inside the cell |
| **Output** | a table you hope is right | a snapshot, machine-verified against a declared shape |
| **Contract** | tribal knowledge | an explicit `interface` — names, types, grain, version |
| **Access** | "ask the data team" | a default-deny policy the cell enforces |

Because the contract is explicit and versioned, cells **compose**: one cell can
consume another cell's published output by name and version — the same way code
libraries depend on each other — without reaching into its internals. You get a
graph of data products, each with a stable public surface and private guts.

---

## What is a cell?

A cell is a small project directory. It represents the basic unit of value.
Developing a cell follows a traditional gitops SDLC.

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
    prod.yaml      # catalog DSN, S3 creds.                               [gitignored]
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

- **[Kubernetes deployment guide](docs/kubernetes.md)**

---

## Install / build

Datamake is a single Rust binary. Building it requires the Rust toolchain
(`rustup`); the first build compiles a bundled DuckDB and is slow, after which
builds are incremental.

```bash
cargo build --release
```

---
