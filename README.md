# Datamake

**Build data products you can ship, version, and trust — not just pipelines you have to babysit.**

Datamake (`datamk`) lets you package a transform, the data it produces, and the
promise of what that data looks like into one self-contained, deployable unit
called a **cell**. Run it anywhere, serve it over HTTP, and evolve it without
breaking the people who depend on it.

---

## What's a composable data product?

A *data pipeline* moves and transforms data. It's plumbing — valuable, but it
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

## The mental model

A cell is a small project directory. You author the contract; Datamake handles
the rest of the lifecycle.

```
   you write                datamk does
  ┌─────────────┐          ┌──────────────────────────────────────────┐
  │ cell.yaml   │  ──run──▶ │ execute transforms → commit a snapshot   │
  │  sources    │          │ → auto-verify against the interface      │
  │  transforms │          ├──────────────────────────────────────────┤
  │  interface  │ ──serve─▶ │ expose the interface as REST + OpenAPI   │
  │  access     │          ├──────────────────────────────────────────┤
  │ sql/*.sql   │ ─publish─▶│ pin this snapshot as the supported       │
  └─────────────┘          │ contract other cells can depend on       │
                           └──────────────────────────────────────────┘
```

- **`cell.yaml`** is the contract: what goes in, what comes out, who may read it.
  It carries *no environment config* — the same cell runs on your laptop and in
  prod, unchanged.
- **Profiles** (`profiles/<name>.yaml`) hold the environment-specific bindings —
  where the catalog and storage live, S3 credentials, etc. — and are selected at
  runtime with `--profile`.
- Data lands in an embedded **DuckLake** (DuckDB + a lake catalog), so a cell is
  self-contained and runs with zero external services to start.

---

## Quickstart

```bash
# 1. Scaffold a cell (writes cell.yaml, a local profile, and example SQL)
datamk init orders

# 2. Run it: execute the transforms, commit a snapshot, auto-verify the interface
datamk run -f orders/cell.yaml

# 3. Serve it: the interface becomes a REST API + OpenAPI spec
datamk serve -f orders/cell.yaml
#   GET http://localhost:8080/orders_daily@2?region=us-east
#   GET http://localhost:8080/openapi.json
```

The scaffold runs end-to-end with **no external setup** — the example transforms
synthesize their own rows — so you can see the full lifecycle before pointing a
cell at real data.

---

## Anatomy of a cell

```yaml
# cell.yaml — the whole contract, and nothing about the environment
cell: orders

sources:                       # external inputs, bound by name before transforms run
  raw_orders: ${ORDERS_PATH:-s3://acme-lake/orders/*.parquet}

transforms:                    # private logic; run in order, atomically → one snapshot
  - sql/stg_orders.sql
  - sql/orders_daily.sql

interface:                     # the public surface — the single source of truth
  - name: orders_daily
    version: 2.1.0             # semver; the route keys on MAJOR → /orders_daily@2
    grain: [order_date, region]  # filterable params + uniqueness-checked by `verify`
    schema:
      order_date: date
      region: string
      revenue: decimal
    contract: experimental     # `datamk publish` promotes this to `supported`

access:                        # default-deny: served only when shareable is true
  shareable: true
  # roles: [analyst]           # if set, callers need a bearer token mapped to a role
```

Two ideas do most of the work:

- **The interface is the seam.** Transforms are private; only what's listed under
  `interface` is exposed. `verify` machine-checks the real output against this
  declaration, so the contract can't silently drift from reality.
- **Publishing is the one deliberate promotion.** Everything is `experimental`
  until you `datamk publish`, which pins a snapshot as the `supported` contract.
  That's the version other cells — and other teams — are allowed to build on.

---

## The CLI

| Command | Does |
|---|---|
| `datamk init <name>` | Scaffold a new cell. |
| `datamk run` | Execute the transforms, commit a snapshot, auto-verify. |
| `datamk verify` | Machine-check actual output against the declared interface. |
| `datamk publish` | Pin the current snapshot as the supported contract. |
| `datamk serve` | Serve the interface as REST + OpenAPI. |

---

## Install / build

Datamake is a single Rust binary. Building it requires the Rust toolchain
(`rustup`); the first build compiles a bundled DuckDB and is slow, after which
builds are incremental.

```bash
cargo build --release
```

---
