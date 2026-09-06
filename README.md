# Datamake

`datamk` packages a transform, the data it produces, and a contract for that
data into one deployable unit called a **cell**. Build it, serve it over
HTTP, and version it without breaking consumers. Apache 2.0, no paid tier.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/scalecraft-dev/datamake/main/install.sh | sh
```

macOS Apple Silicon and Linux x86_64/arm64 (glibc 2.28+). Windows: run the
same line in WSL2. Anything else: `cargo install --git https://github.com/scalecraft-dev/datamake datamk`
(the first build compiles DuckDB and is slow).

## Quick start

```bash
datamk init orders && cd orders
datamk run -f cell.yaml        # run transforms, commit one snapshot, verify it
datamk serve -f cell.yaml      # GET /orders_daily@2?region=us-east, /openapi.json, /context
```

`init` scaffolds this:

```
orders/
  cell.yaml          # contract: sources, transforms, interface, access
  sql/*.sql          # transforms, run in order → one atomic snapshot
  profiles/local.yaml  # laptop bindings; profiles/prod.yaml is gitignored
  deploy/prod.yaml   # how the workloads run in prod
```

A minimal `cell.yaml`:

```yaml
cell: orders
sources:
  raw_orders: ${ORDERS_PATH:-s3://acme-lake/orders/*.parquet}
transforms:
  - sql/stg_orders.sql
  - sql/orders_daily.sql
interface:
  - name: orders_daily
    version: 2.1.0                # route keys on MAJOR → /orders_daily@2
    grain: [order_date, region]   # query filters; uniqueness-checked by verify
    schema: { order_date: date, region: string, revenue: decimal }
    contract: experimental        # experimental | supported
access:
  shareable: true                 # default-deny
```

`cell.yaml` holds no environment config. `-p <profile>` selects
`profiles/<profile>.yaml` (default `local`).

## Commands

| Command | Does |
| --- | --- |
| `init <name>` | Scaffold a cell. `--from sqlmesh` for a [discovered cell](docs/guides/discover.md). |
| `run` | Execute transforms, commit a snapshot, verify. `--full-refresh`, `--verify-replay`. |
| `verify` | Check actual output against the declared interface. |
| `release` | Pin the current snapshot as the supported contract. |
| `rollback` | Repoint the served data at an earlier execution. |
| `status` | Show published executions and the LATEST pointer. |
| `serve` | REST + OpenAPI + `/context`. See [serving](docs/guides/serving.md). |
| `mcp` | Same interface over MCP stdio. See [mcp](docs/guides/mcp.md). |
| `context` | Emit the context document. See [context](docs/guides/context.md). |
| `mesh emit` | Emit the static manifest listing cells. |
| `sync` | Read a modeling tool's deployed state into the cell. |
| `interface` | Import a warehouse object's types into an export block. |
| `attach` | Print SQL that attaches the cell's catalog read-only in DuckDB. |
| `deploy` | Run the cell on an orchestrator. See [kubernetes](docs/guides/kubernetes.md). |

`run`, `release`, `rollback`, and `deploy` write one log per invocation to
`.cell/logs/`. `--log-dir`, `--log-keep` (default 20), `DATAMK_LOG=off`.

## Docs

- [Sources](docs/guides/sources.md): files, other cells, BigQuery, Postgres, Snowflake, `query:`, `incremental:`.
- [Serving](docs/guides/serving.md): query grammar, status codes, multi-cell projects, throttling.
- [Context document](docs/guides/context.md): the interface made machine-readable for agents.
- [Why composable data products?](docs/concepts/composable-data-products.md)
- [Design decisions (ADRs)](docs/adr/)
