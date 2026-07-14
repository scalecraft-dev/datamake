use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::cli::InitArgs;

/// Scaffold a new cell — an implementation project: the contract (`cell.yaml`), a
/// runnable local profile, a deployable `prod` profile + tracked deploy overlay,
/// and the SQL.
pub fn run(args: InitArgs) -> Result<()> {
    let dir = args.path.unwrap_or_else(|| PathBuf::from(&args.name));
    if dir.exists() {
        bail!("{} already exists", dir.display());
    }
    std::fs::create_dir_all(dir.join("sql"))?;
    std::fs::create_dir_all(dir.join("profiles"))?;
    std::fs::create_dir_all(dir.join("deploy"))?;

    write(&dir.join("cell.yaml"), &cell_yaml(&args.name))?;
    write(&dir.join("profiles/local.yaml"), PROFILE_LOCAL)?;
    write(&dir.join("profiles/prod.yaml"), &profile_prod(&args.name))?;
    write(&dir.join("deploy/prod.yaml"), DEPLOY_PROD)?;
    write(&dir.join("sql/stg_orders.sql"), STG_ORDERS_SQL)?;
    write(&dir.join("sql/order_totals.sql"), ORDER_TOTALS_SQL)?;
    write(&dir.join("sql/orders_daily.sql"), ORDERS_DAILY_SQL)?;
    write(&dir.join(".gitignore"), GITIGNORE)?;
    write(&dir.join("README.md"), &readme(&args.name))?;

    let d = dir.display();
    println!("Created cell '{}' in {d}", args.name);
    println!("Next:");
    println!("  datamk run    -f {d}/cell.yaml   # build a snapshot, then auto-verify");
    println!("  datamk serve  -f {d}/cell.yaml   # serve the interface at http://localhost:8080");
    println!("Ship it (edit profiles/prod.yaml + deploy/prod.yaml first):");
    println!("  datamk deploy -f {d}/cell.yaml -p prod --dry-run");
    Ok(())
}

fn write(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

fn cell_yaml(name: &str) -> String {
    format!(
        r#"cell: {name}

# sources:                        # external inputs, bound as TEMP VIEWs before transforms.
#   raw_orders: ${{ORDERS_PATH:-s3://acme-lake/orders/*.parquet}}   # DuckDB auto-detects format
#   crm_accounts:                 # a warehouse table via a named connection
#     connection: crm             # -> profiles/<name>.yaml `connections.crm`
#     table: sales.accounts       # which table is contract; which project is environment
#     # a view source auto-routes to the BigQuery jobs API (no pushdown; add incremental: for deltas)
#     incremental:                # read only rows past a watermark each run (ADR 0005)
#       cursor: updated_at        # a monotonic column (timestamp/date/integer); its max is the mark
#       # lookback: 2h            # optional: also re-read a trailing window for late rows (time cursors only)
#     # A transform on an incremental source MUST be replay-safe. There is one language for
#     # transforms (ADR 0008): every file is a SELECT, and `materialize: upsert|append` (like
#     # sql/order_totals.sql below) is how you accumulate one — the engine composes the
#     # state-transition DML for you, so there is no hand-written CREATE/MERGE to get wrong.
#     # `materialize: replace` (the default, used below for stg_orders/orders_daily) is fine
#     # over a NON-incremental source; over an incremental one it is a resolve-time error the
#     # moment the file's SQL text references the source's name (rebuilding from just the delta
#     # replaces history with it) — read the accumulated table instead, or make the model itself
#     # an upsert/append. See docs/guides/incremental.md.
#   raw_spend_hourly:              # `query:` instead of `table:` — server-side shaping (ADR 0007)
#     connection: crm              # when scan-with-pushdown can't express it (e.g. GROUP BY);
#     query: |                     # exactly-one-of with `table:`; never composes with incremental:
#       SELECT advertiser_id, hour, CAST(SUM(spend) AS NUMERIC) AS spend
#       FROM `${{connection.project}}.dataset.minute_grain_view`  -- reserved binding; never
#       GROUP BY 1, 2                                             -- hardcode a project literally
#     # CAST(... AS NUMERIC): BigQuery BIGNUMERIC/full-range NUMERIC degrade silently to VARCHAR
#     # past DuckDB's DECIMAL(38,.) — the CAST is an author correctness assertion, covered by a
#     # gated warehouse correctness test (tests/bigquery_query_correctness.rs), not an engine check.
                                  # transforms then read sources by name.

transforms:                       # private; run in listed order, atomically -> one snapshot
  - sql/stg_orders.sql            # bare path (ADR 0008): SELECT-only file, replace strategy
                                   # implied by default — rebuilds from scratch every run (no key)
  - sql: sql/order_totals.sql     # declarative shape: an accumulator this time, not a rebuild
    materialize: upsert           # append | upsert; both replay-safe by construction
    key: [order_id]               # one row per key; table name defaults to the file stem
  - sql/orders_daily.sql          # a rollup over order_totals — rebuild each run is the correct
                                   # shape, so this is a bare path (replace, implied) too

interface:                        # the export list - the public surface, single source of truth
  - name: orders_daily
    version: 2.1.0                # semver; route keys on MAJOR -> GET /orders_daily@2
    source: orders_daily          # physical object in the lake (defaults to name)
    grain: [order_date, region]   # filterable query params + uniqueness-checked by `verify`
    schema:
      order_date: date
      region: string
      revenue: decimal
    freshness: daily
    visibility: discoverable      # private | discoverable
    contract: experimental        # experimental | supported  (a reviewed edit; `release` then pins it)

access:                           # default-deny: the serving plane exposes data only when shareable
  shareable: true
  # roles: [analyst]              # if set, callers need a bearer token mapped to one of these roles

# Bindings (catalog/storage/s3/principals) are NOT here — they are environment-
# specific. They live in profiles/<name>.yaml, selected with `--profile`.
"#
    )
}

/// The committed local profile: zero-config defaults so the cell runs after clone.
/// Other profiles (prod, staging) carry secrets and are gitignored.
const PROFILE_LOCAL: &str = r#"# Binding profile: local. Selected by default (`datamk run`).
# Environment-specific; values may reference ${VARS} for secrets.
catalog: ./.cell/catalog.ducklake    # local-dev catalog file; deployed profiles OMIT
                                     # catalog: entirely (derived from storage, ADR 0004)
storage: ./.cell/data                # file:// -> s3:// -> gs://
# principals: ./.cell/principals.json   # token -> roles (only when access.roles is set)
# s3:                                # only when storage/sources are s3://
#   region: ${AWS_REGION:-us-east-1} # omit key_id/secret to use the AWS credential chain
#   # endpoint: ${S3_ENDPOINT}       # MinIO / Cloudflare R2
#   # url_style: path
#   # key_id: ${AWS_ACCESS_KEY_ID}
#   # secret: ${AWS_SECRET_ACCESS_KEY}
# gcs:                               # only when storage/sources are gs://
#   # credentials: secrets/gcs-key.json  # SA key file; omit to use ambient ADC
#   key_id: ${GCS_HMAC_KEY_ID}       # HMAC pair (`gcloud storage hmac create`) — DuckDB's
#   secret: ${GCS_HMAC_SECRET}       #   built-in gs:// reads use these, never ADC
#   # extension: vendor/gcs.duckdb_extension  # native GCS extension: keyless (ADC) DuckDB
#   #                                # reads, no HMAC needed (northpolesec/duckdb-gcs)
"#;

/// A deployable `prod` profile (gitignored). Storage only — a deployed cell's
/// sole external dependency (ADR 0004) — fill in real values.
fn profile_prod(name: &str) -> String {
    format!(
        r#"# Binding profile: prod. Gitignored — carries s3 creds and (when access.roles
# is set) the principals path. A DEPLOYABLE profile: a shared object store and
# NO catalog: — a deployed cell derives its catalog from storage and publishes
# an immutable catalog artifact per execution (ADR 0004). catalog: is
# local-dev only; deploy pre-flight rejects it here.
storage: s3://your-bucket/cells/{name}      # shared object store, not ./.cell
# principals: /etc/datamk/principals.json   # token -> roles; set when access.roles is used (secret mount)
s3:
  region: ${{AWS_REGION:-us-east-1}}        # omit key_id/secret to use the AWS credential chain
  # key_id: ${{AWS_ACCESS_KEY_ID}}
  # secret: ${{AWS_SECRET_ACCESS_KEY}}
# gcs:                                      # GCS instead: storage: gs://your-bucket/cells/{name}
#   key_id: ${{GCS_HMAC_KEY_ID}}            # HMAC pair (`gcloud storage hmac create`) — DuckDB's
#   secret: ${{GCS_HMAC_SECRET}}            #   built-in gs:// reads use these, never ADC
#   # extension: /opt/datamk/gcs.duckdb_extension  # native GCS extension: keyless (ADC)
#   #                                       # DuckDB reads, no HMAC (northpolesec/duckdb-gcs)
#   # credentials: /etc/datamk/gcs-key.json # SA key file; omit for
#   #                                       # ambient ADC / workload identity
# connections:                              # named warehouse connections (`connection` sources)
#   crm:
#     type: bigquery
#     project: ${{GCP_PROJECT}}             # the project whose datasets are read
#     # billing_project: ${{GCP_BILLING_PROJECT}}   # defaults to `project`
#     # credentials: /etc/datamk/bq-key.json        # SA key file (secret mount); omit to use ADC
"#
    )
}

/// The tracked, secret-free deploy overlay for `prod`.
const DEPLOY_PROD: &str = r#"# deploy/prod.yaml — deploy topology for the `prod` profile.
# TRACKED and PR-reviewed: this is HOW/WHERE the workload runs. It has NO secret
# fields by design — credentials live in profiles/prod.yaml (gitignored). Keeping
# topology in a separate tracked file keeps secrets out of a reviewed file.
target: kubernetes              # the orchestrator (only `kubernetes` implemented)

# allow_anonymous: false        # TOP-LEVEL. true => deploy a deliberately open,
                                # unauthenticated endpoint (cell shareable, no roles).

# Target-specific topology is defined by the target's ADR — see ADR 0002 for the
# Kubernetes schema. Sketch:
#   namespace: data
#   schedule: "0 * * * *"       # Builder cron — each run publishes an execution (ADR 0004)
#   retention_days: 30          # compaction window; also the rollback horizon (ADR 0004 §10)
#   serve:
#     replicas: 2               # each replica holds its own catalog copy (ADR 0004)
#     poll_interval: 15         # seconds between LATEST checks = staleness bound
#   image: ghcr.io/scalecraft-dev/datamk   # tag defaults to the running datamk version
"#;

const STG_ORDERS_SQL: &str = r#"-- One language for transforms (ADR 0008): SELECT-only — no CREATE, no
-- trailing `;` (the engine wraps this file's text verbatim as a
-- parenthesized subquery, so a trailing semicolon breaks it). This is a
-- bare-path entry in cell.yaml, so `materialize: replace` is implied: the
-- engine wraps this SELECT in `CREATE OR REPLACE TABLE stg_orders AS
-- (<this file>)`, rebuilding it from scratch every run. `replace` is fine
-- here because this SELECT reads no incremental source — if it did, the
-- engine would refuse it at resolve time (guard 4c): a rebuild from a delta
-- alone would replace accumulated history with just that delta.
-- In a real cell this SELECT would read source tables from the lake; here we
-- synthesize rows so `datamk run` works with zero external setup.
SELECT * FROM (VALUES
    (1, DATE '2026-06-01', 'us-east', 120.50),
    (2, DATE '2026-06-01', 'us-east',  80.00),
    (3, DATE '2026-06-01', 'us-west', 200.25),
    (4, DATE '2026-06-02', 'us-east',  59.99),
    (5, DATE '2026-06-02', 'eu-west', 410.00)
) AS t(order_id, order_date, region, amount)"#;

const ORDER_TOTALS_SQL: &str = r#"-- One language for transforms (ADR 0008): SELECT-only — no CREATE, no
-- MERGE, no ANTI JOIN, and (important) NO TRAILING SEMICOLON: the engine
-- wraps this file's text verbatim as `(<this file>)`, a parenthesized
-- subquery, so a trailing `;` breaks the wrapping statement's syntax — the
-- engine never parses or rewrites the SELECT to fix this for you.
-- The `materialize: upsert` entry in cell.yaml wraps this in the
-- state-transition DML: CREATE TABLE IF NOT EXISTS, then a MERGE keyed on
-- order_id. This is the required shape for a transform over an
-- incremental source (see the commented `incremental:` block above) —
-- the engine, not the author, owns replay-safety, so there is no
-- CREATE OR REPLACE footgun to avoid here at all. `key: [order_id]` is
-- also this table's grain: an export sourced from order_totals can omit
-- `grain:` entirely and inherit it (see docs/guides/incremental.md).
SELECT order_id, order_date, region, amount
FROM stg_orders"#;

const ORDERS_DAILY_SQL: &str = r#"-- Public export `orders_daily@2`. Grain (order_date, region) must be unique —
-- `datamk verify` enforces that against this actual output. One language for
-- transforms (ADR 0008): SELECT-only, no trailing `;`. This is a bare-path
-- entry in cell.yaml, so `materialize: replace` is implied: the engine wraps
-- this in `CREATE OR REPLACE TABLE orders_daily AS (<this file>)` — the
-- correct shape for a rollup rebuilt from scratch every run, over
-- order_totals (an accumulator, not the incremental source directly — guard
-- 4c would refuse that). This export declares `grain:` explicitly
-- (cell.yaml) rather than inheriting one: `replace` has no `key:`, so there
-- is nothing to inherit from (see docs/guides/incremental.md §4).
SELECT
    order_date,
    region,
    SUM(amount)::DECIMAL(18,2) AS revenue
FROM stg_orders
GROUP BY order_date, region"#;

const GITIGNORE: &str = "\
# Generated, derived state: local catalog, Parquet data, release manifest.
.cell/
# Binding profiles carry environment config / secrets — keep only `local`.
profiles/*
!profiles/local.yaml
# deploy/ is tracked on purpose: topology is PR-reviewed and secret-free. Do not ignore it.
";

fn readme(name: &str) -> String {
    format!(
        r#"# {name}

A [datamk](https://github.com/scalecraft/datamk) cell.

```
datamk run     -f cell.yaml          # execute the pipeline -> snapshot -> verify
datamk serve   -f cell.yaml          # GET /orders_daily@2 , /openapi.json , /interface
datamk release -f cell.yaml          # pin the current snapshot as the supported contract
datamk deploy  -f cell.yaml -p prod  # run the Builder + Server on an orchestrator
```

- `cell.yaml` — the contract: sources, transforms, interface, access. No environment.
- `profiles/<name>.yaml` — environment bindings (storage/s3/principals; `catalog:` is local-dev only). Pick with `--profile`.
- `deploy/<name>.yaml` — deploy topology (target, schedule, replicas). Tracked and PR-reviewed; secret-free.
- `sql/` — private transform logic.
- `.cell/` — generated catalog + data + release manifest (gitignored).

Promotion is a review, not a command: edit an export to `contract: supported`,
open a PR, and once it lands `datamk release` pins that snapshot. Deploy with
`datamk deploy -p prod` once `profiles/prod.yaml` and `deploy/prod.yaml` are filled in.
"#
    )
}
