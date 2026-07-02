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
                                  # transforms then read sources by name.

transforms:                       # private; run in listed order, atomically -> one snapshot
  - sql/stg_orders.sql
  - sql/orders_daily.sql

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
catalog: ./.cell/catalog.ducklake    # file -> sqlite: -> postgres:
storage: ./.cell/data                # file:// -> s3:// -> gs://
# principals: ./.cell/principals.json   # token -> roles (only when access.roles is set)
# s3:                                # only when storage/sources are s3://
#   region: ${AWS_REGION:-us-east-1} # omit key_id/secret to use the AWS credential chain
#   # endpoint: ${S3_ENDPOINT}       # MinIO / Cloudflare R2
#   # url_style: path
#   # key_id: ${AWS_ACCESS_KEY_ID}
#   # secret: ${AWS_SECRET_ACCESS_KEY}
"#;

/// A deployable `prod` profile (gitignored). Defaults to a metadata-DB catalog +
/// shared object store, the shape `datamk deploy` requires — fill in real values.
fn profile_prod(name: &str) -> String {
    format!(
        r#"# Binding profile: prod. Gitignored — carries the catalog DSN, s3 creds, and
# (when access.roles is set) the principals path. A DEPLOYABLE profile: a
# metadata-DB catalog + a shared object store, which `datamk deploy` requires.
catalog: ${{DATAMK_CATALOG}}                # sqlite:… (single replica) or postgres:… (multi-replica)
storage: s3://your-bucket/cells/{name}      # shared object store, not ./.cell
# principals: /etc/datamk/principals.json   # token -> roles; set when access.roles is used (secret mount)
s3:
  region: ${{AWS_REGION:-us-east-1}}        # omit key_id/secret to use the AWS credential chain
  # key_id: ${{AWS_ACCESS_KEY_ID}}
  # secret: ${{AWS_SECRET_ACCESS_KEY}}
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
#   schedule: "0 * * * *"       # Builder cron (datamk run)
#   serve:
#     replicas: 2               # >1 requires a `postgres:` catalog
#   image: ghcr.io/scalecraft/datamk   # tag defaults to the running datamk version
"#;

const STG_ORDERS_SQL: &str = r#"-- Private internal. In a real cell this reads source tables from the lake;
-- here we synthesize rows so `datamk run` works with zero external setup.
CREATE OR REPLACE TABLE stg_orders AS
SELECT * FROM (VALUES
    (1, DATE '2026-06-01', 'us-east', 120.50),
    (2, DATE '2026-06-01', 'us-east',  80.00),
    (3, DATE '2026-06-01', 'us-west', 200.25),
    (4, DATE '2026-06-02', 'us-east',  59.99),
    (5, DATE '2026-06-02', 'eu-west', 410.00)
) AS t(order_id, order_date, region, amount);
"#;

const ORDERS_DAILY_SQL: &str = r#"-- Public export `orders_daily@2`. Grain (order_date, region) must be unique --
-- `datamk verify` enforces that against this actual output.
CREATE OR REPLACE TABLE orders_daily AS
SELECT
    order_date,
    region,
    SUM(amount)::DECIMAL(18,2) AS revenue
FROM stg_orders
GROUP BY order_date, region;
"#;

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
- `profiles/<name>.yaml` — environment bindings (catalog/storage/s3/principals). Pick with `--profile`.
- `deploy/<name>.yaml` — deploy topology (target, schedule, replicas). Tracked and PR-reviewed; secret-free.
- `sql/` — private transform logic.
- `.cell/` — generated catalog + data + release manifest (gitignored).

Promotion is a review, not a command: edit an export to `contract: supported`,
open a PR, and once it lands `datamk release` pins that snapshot. Deploy with
`datamk deploy -p prod` once `profiles/prod.yaml` and `deploy/prod.yaml` are filled in.
"#
    )
}
