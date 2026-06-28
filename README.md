# Datamake - Composable Data Products

Deploy self-contained data pipelines that create value for particular use cases.

## CLI

| Command | Workload | Does |
|---|---|---|
| `datamk init <name>`  | — | Scaffold a new cell (an implementation project). |
| `datamk run`          | Builder | Execute the transform pipeline, commit a snapshot, auto-verify. |
| `datamk verify`       | — | Machine-check actual output against the declared interface. |
| `datamk publish`      | — | Pin the current snapshot as the supported contract. |
| `datamk serve`        | Server | Serve the interface as REST + OpenAPI. |

## Quickstart

```bash
datamk init orders
datamk run   -f orders/cell.yaml
datamk serve -f orders/cell.yaml
# GET http://localhost:8080/orders_daily@2?region=us-east
# GET http://localhost:8080/openapi.json
```

A runnable cell lives in [`examples/orders`](examples/orders).

## Sources & object storage

A cell reads external inputs declared under `sources` (name → URI). Each is bound as
a session-local `TEMP VIEW` before transforms run, so transforms reference a stable
name while the path stays injectable — and the external source never leaks into the
DuckLake snapshot. DuckDB reads Parquet/CSV/JSON (and globs) and auto-detects format.

```yaml
# cell.yaml — sources are part of the contract
sources:
  raw_orders: ${ORDERS_PATH:-s3://acme-lake/orders/*.parquet}
```

```yaml
# profiles/prod.yaml — environment bindings (gitignored), picked with --profile prod
storage: s3://acme-lake/cells/orders
s3:
  region: ${AWS_REGION:-us-east-1}
  # endpoint / url_style for MinIO, Cloudflare R2, etc.
  # key_id / secret for static credentials
```

S3 uses DuckDB's `httpfs` + Secrets Manager. With no `key_id`/`secret`, the
`credential_chain` provider resolves AWS env vars, shared profiles, and IAM roles —
no secrets in the cell config. Set `endpoint`/`url_style: path` for S3-compatible
stores (MinIO, R2). `gs://` works via the same `httpfs` path.

## Build

Requires the Rust toolchain (`rustup`). The first build compiles a bundled DuckDB,
so expect it to be slow; subsequent builds are incremental.

```bash
cargo build --release
```

## Layout

```
src/
  main.rs        cli.rs        # entry + argument parsing
  config/        # cell.yaml model + ${VAR:-default} binding resolution
  engine/        # DuckDB connection, DuckLake attach, transform runner (`run`)
  verify.rs      # interface <-> actual-output checks
  publish.rs     # snapshot-pinning contract promotion
  serve/         # axum REST + OpenAPI generated from the interface
```

Status: v0, pre-release. The cell definition is the one primitive we are spending
design budget on — it is a contract the day it ships.
