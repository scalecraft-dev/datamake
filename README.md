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
    prod.yaml      # storage + object-store creds (no catalog — ADR 0004) [gitignored]
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
OpenAPI: `GET /orders_daily@2?region=us-east`, `GET /openapi.json`. The query
grammar is closed and deterministic — grain filters, ordered pages, unknown
params are a `400`, never silently ignored
([serving guide](docs/guides/serving.md)). And `GET /context` serves the
whole boundary — meaning included — as one document agents can trust,
because the build verifies what it describes
([context guide](docs/guides/context.md),
[ADR 0012](docs/adr/0012-cell-context-document.md)).

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

*Which table* is contract (`cell.yaml`); *which warehouse, project, and
credentials* is environment — the profile supplies the connection, so the
same cell reads a sandbox in dev and the real warehouse in prod.

Connection sources can also declare **`query:`** (shaping SQL executed
server-side, in the warehouse's own dialect) and **`incremental:`** (a
cursor column, so each run reads only rows past the persisted watermark;
delivery is at-least-once, so transforms over incremental sources must be
replay-safe).

Connector setup and per-warehouse behavior live in the guides:
[Sources](docs/guides/sources.md) ·
[Postgres](docs/guides/postgres.md) ·
[Snowflake](docs/guides/snowflake.md) ·
[Incremental loading](docs/guides/incremental.md)

---

## The CLI

| Command | Does |
| --- | --- |
| `datamk init <name>` | Scaffold a new cell. |
| `datamk run` | Execute the transforms, commit a snapshot, auto-verify. |
| `datamk verify` | Machine-check actual output against the declared interface. |
| `datamk release` | Pin the current snapshot as the supported contract. |
| `datamk serve` | Serve the interface as REST + OpenAPI + `/context`. |
| `datamk context` | Emit the cell's context document — the interface made machine-readable for agents. |
| `datamk mcp` | Serve the interface to an MCP client over stdio: three tools over the same closed grammar as `serve`. |
| `datamk mesh emit` | Emit the static manifest that tells an agent which cells exist. |
| `datamk deploy` | Run the cell as managed workloads on an orchestrator. |
| `datamk attach` | Print SQL that attaches the cell's catalog in DuckDB, read-only. |

`datamk attach` prints a stateless, portable recipe — runnable on any host
with credentials (`datamk attach --help` documents the one storage-specific
exception and its `--download` escape hatch).

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
