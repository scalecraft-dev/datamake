# ADR 0003 — Generic source definitions: warehouse connections

- **Status:** Proposed
- **Date:** 2026-07-02
- **Deciders:** Datamake team
- **Author:** @scottypate

## Scope

This ADR defines the **connector-agnostic source contract**: a third source
kind in `cell.yaml` (`connection` sources), a `connections:` map in the binding
profile, the engine seam every connector implements, and the invariants
(trust boundary, read-only, resolve-time validation) all of them uphold. It
also specifies the **first connector: BigQuery**, which is small enough to
live here and serves as the proving realization. Later warehouses (Snowflake,
Redshift, Databricks) are additive connectors against this same contract; any
whose realization is non-trivial gets its own ADR, as Kubernetes (ADR 0002)
does for the deploy contract (ADR 0001).

## Context

A cell today has exactly two kinds of external input (`Source` in
`src/config/schema.rs`):

- **Raw** — a path/URI string (`s3://…`, `gs://…`, local files, globs) that
  DuckDB reads directly via httpfs; and
- **Cell** — another cell's DuckLake table, attached read-only by catalog +
  storage location and read by name.

That covers files-in-object-storage and datamake-managed tables. It does not
cover the place most teams' upstream data actually lives: a data warehouse.
Today the only way to source BigQuery (or Snowflake, Redshift, …) data into a
cell is an out-of-band export to Parquet in a bucket — an extra pipeline,
an extra copy, and a staging location datamake knows nothing about.

Two existing seams shape the design:

- **The contract/environment split.** `Source::Cell` already models it: the
  reference name, table, and version pin are contract (`cell.yaml`); the
  upstream's physical location is environment (`profiles/<name>.yaml`, the
  `cells:` map). Warehouse sources have the same split — *which table* is
  contract, *which project/account and credentials* is environment.
- **The scanner-extension mechanism.** The engine already attaches foreign
  databases through DuckDB scanner extensions: metadata-DB catalogs load
  `postgres`/`sqlite` (`engine::load_catalog_extension`), and cell sources
  are `ATTACH … READ_ONLY` + a session-local TEMP VIEW (`engine::bind_source`).
  Warehouse scanners (e.g. the community `bigquery` extension) plug into
  exactly this mechanism — no new execution model is needed.

## Decision

Add a third source kind, `connection` sources, resolved through a named
`connections:` map in the profile, and bound by the engine through DuckDB
scanner extensions using the same attach-read-only-plus-TEMP-VIEW mechanism
cell sources already use. BigQuery is the first connector.

### 1. `cell.yaml`: the `connection` source kind

A connection source names a connection (resolved via the profile) and a table
within it:

```yaml
sources:
  raw_orders: s3://acme/orders/*.parquet     # Raw — unchanged
  upstream:                                  # Cell — unchanged
    cell: customers
    table: dim_customers
  crm_accounts:                              # Connection — new
    connection: crm
    table: sales.accounts
```

- `connection` is a reference name, resolved via the profile's
  `connections:` map — exactly parallel to how `cell:` resolves via `cells:`.
- `table` is the connector-scoped table path. Its shape is defined and
  validated per connector (BigQuery: `dataset.table`, §5). The connection's
  environment root (BigQuery: the project) is **not** part of it — the same
  `cell.yaml` reads a sandbox project in dev and the real one in prod.

`Source` stays `#[serde(untagged)]`; the three variants are disambiguated by
their required keys (a bare string, `cell:`, `connection:`), which are
disjoint. Existing cell definitions parse unchanged.

### 2. Profile: the `connections:` map

Connections are environment config and live in the binding profile, keyed by
the reference name and internally tagged by `type:`:

```yaml
# profiles/prod.yaml (gitignored, like every non-local profile)
catalog: postgres://…
storage: s3://acme-lake/orders
connections:
  crm:
    type: bigquery
    project: acme-prod-crm
    # billing_project: acme-data-eng        # optional; defaults to `project`
    # credentials: /var/run/secrets/bq/key.json   # optional; ADC otherwise
```

- `type:` is a closed enum (`bigquery` only, initially). An unknown type is a
  parse error that names the valid types — the same fail-loud posture as
  `--target` in ADR 0001.
- Every field is `${VAR}`-expandable via the existing `bindings::expand`,
  like the rest of the profile.
- Credential material is referenced **by path or ambient chain, never as a
  literal token field** where the connector's auth model allows it (BigQuery
  does, §5) — mirroring `principals:`, and keeping new literal-secret surface
  out of the schema even though profiles are already the gitignored,
  secret-bearing file.

A source referencing an undefined connection fails at **resolve time**
(`config::resolve`), naming the source and the missing `connections.<name>`
entry — the same behavior and error shape as a missing `cells.<name>`
location. No database is opened to discover the mistake, so `deploy` (which
uses `config::load`) surfaces it in pre-flight for free.

### 3. Engine: connectors are attach recipes, not a new execution path

`ResolvedSource` gains a `Connection` variant carrying the resolved connection
config inline (as `Cell` inlines its location). `engine::bind_source` realizes
it with the mechanism cell sources already use:

1. `LOAD` the connector's scanner extension(s);
2. `ATTACH IF NOT EXISTS '<connector-specific string>' AS __conn_<name>
   (TYPE <type>, READ_ONLY);` — attached **once per connection**, shared by
   every source that references it;
3. `CREATE OR REPLACE TEMP VIEW "<source>" AS SELECT * FROM
   __conn_<name>.<qualified table>;`

A connector is therefore three answers, not a subsystem:

```rust
// One impl per Connection variant (match arms — connectors are DuckDB
// extensions, not Rust dependencies, so unlike DeployTarget there is no
// cargo-feature weight to isolate and no dyn dispatch needed).
//   extensions() -> &[&str]                   // e.g. ["bigquery"]
//   attach_sql(alias) -> String               // the ATTACH statement
//   qualify(alias, table) -> Result<String>   // validate + quote the table path
```

Adding a warehouse is a new `Connection` variant plus these three answers —
no change to `run`, `bind_source`'s structure, `serve`, or `verify`.

Invariants every connector upholds:

- **Read-only.** Connections attach `READ_ONLY`. Sources are inputs; datamake
  never writes to a warehouse.
- **Session-local.** The TEMP VIEW is visible to transforms and never
  committed to the catalog — identical to the other two source kinds. The
  lake remains the only thing datamake persists; `serve` and the contract
  surface are untouched by where a source came from.
- **Declared inputs only.** Transforms reach warehouse data exclusively
  through declared source views; the attached connection alias
  (`__conn_<name>`) is engine-internal, not a documented surface. This costs
  nothing in execution — DuckDB **inlines view definitions into the plan**,
  so a filter written against the view pushes into the scanner exactly as if
  the transform addressed `__conn_<name>.<table>` directly — and it is what
  keeps `cell.yaml` a complete, reviewable input manifest: lineage without
  parsing SQL, table typos caught at resolve time instead of mid-transaction,
  and `dataset.table` named in one place so transform SQL stays portable
  across environments. (The pushdown half of this argument holds for
  storage-scannable base tables only — jobs-API-routed objects have no
  scanner to push into; see ADR 0006.)
- **Read-through semantics.** Each `run` reads the source through the scanner
  at query time. Projections and filters written in transform SQL push down
  (through the inlined view) where the extension supports it; there is no
  local caching or incrementality in v1 (a consequence, below).

### 4. Deploy implications (against ADR 0001/0002, no new contract)

- **Extensions are baked, not installed.** Per ADR 0001 §5, the base image
  bakes every extension the engine may `LOAD` — connector extensions
  included. The `bigquery` extension comes from DuckDB's **community**
  repository (`INSTALL bigquery FROM community` at image build). Note the
  baking itself is the same deferred follow-up the Dockerfile records for
  ducklake/httpfs (egress-ful pods INSTALL at first run today); connector
  extensions join that follow-up rather than resolving it.
- **Credentials follow the principals pattern.** A connection's credential
  file path (e.g. BigQuery `credentials:`) names an in-cluster mount; the
  deploy host cannot stat it. Pre-flight verifies the *configuration* (the
  profile names a path or the connector's ambient chain applies), the target
  mounts the operator-provided secret at that path (Kubernetes: a `Secret`
  mounted `0400`, ADR 0002), and the engine fails loud at attach if auth is
  absent — the same split-by-where-the-check-can-run as ADR 0001 §8.
- **Egress.** Builder pods need network egress to the warehouse. This is
  documented, not pre-flighted — reachability is only knowable in-cluster,
  and a failed attach already fails the init Job with the build pod's logs
  (ADR 0002 §1).

### 5. First connector: BigQuery

Realized via the DuckDB community `bigquery` extension (which reads through
the BigQuery Storage Read API — for **base tables**; the Storage Read API
cannot read views, materialized views, or external tables, which route
through the jobs API instead, per ADR 0006):

- **Config** (`type: bigquery`): `project` (required — the project whose
  datasets are read), `billing_project` (optional — where query/read costs
  land, defaults to `project`), `credentials` (optional — path to a service
  account key file).
- **Attach:** `ATTACH 'project=<project>' AS __conn_<name>
  (TYPE bigquery, READ_ONLY);` with the billing project included when set.
- **Table shape:** exactly `dataset.table`; the connector rejects one-part
  and three-part names with an error that shows the expected form. A
  cross-project read is a second connection, not a three-part name — one
  connection ≡ one project keeps the environment root in the profile.
- **Auth:** Google **Application Default Credentials**. With `credentials:`
  set, the engine points ADC at that key file for the session; otherwise the
  ambient chain applies (`GOOGLE_APPLICATION_CREDENTIALS`, `gcloud auth
  application-default login`, or the workload identity of the pod). No token
  ever appears in a profile field.
- **Identifiers** are validated and quoted by the connector (`qualify`), the
  same double-quote discipline `bind_source` applies to view and table names
  today.

### 6. `s3:` stays where it is

The existing `s3:` binding is **storage-plane** config: it parameterizes the
httpfs secret used by the lake's `DATA_PATH` *and* every raw `s3://` source.
It is not a per-source connection and does not move into `connections:`.
Folding it in is considered and deferred below.

## Consequences

- Warehouse tables become first-class cell inputs — no out-of-band
  export-to-bucket pipeline, no staging copy.
- Existing `cell.yaml`s and profiles parse unchanged; the new source kind and
  the `connections:` map are purely additive (`#[serde(default)]`).
- New connectors are additive: a `Connection` variant + three connector
  answers + a baked extension. Non-trivial ones get their own ADR.
- The base-image build gains community-repository extensions
  (`INSTALL bigquery FROM community`), pinned with the image version.
- Every `run` re-reads warehouse sources through the scanner. For large
  tables this costs a full (pushdown-filtered) scan per build; incremental
  reads and local caching are explicitly out of scope for v1.
- `verify`, `serve`, snapshot pinning, and the export contract are untouched:
  sources remain session-local inputs to the Builder.
- `datamk init` scaffolding (profile template comments, README) gains a
  commented `connections:` example; an example cell with a BigQuery source is
  added to `datamk-examples`.
- Deploy pre-flight gains the connection checks in §4; the `kind` e2e harness
  is unaffected (no warehouse in the loop) — BigQuery gets an integration
  test gated behind credentials, skipped when absent.

## Alternatives considered

- **A URI scheme as a Raw source** (`bigquery://project/dataset/table`).
  Rejected: it buries environment (the project) in the contract file, gives
  auth/billing config nowhere to live, and a bare string offers no seam for
  per-connector validation. It also breaks the portability that makes cells
  work: the same `cell.yaml` could no longer run against dev and prod.
- **Connection config inline in `cell.yaml`.** Rejected for the same
  trust-boundary reason deploy topology was kept out of profiles (ADR 0001
  §6), inverted: project names and credential paths are environment, and the
  contract file must stay env-free.
- **Open federation** (attach each connection under its reference name and
  let transforms query any table in it directly — `crm.sales.accounts` — with
  `sources:` as optional sugar). Rejected: because views are inlined, direct
  addressing gains *nothing* in execution over a declared source; what it
  loses is the input manifest. Cell inputs would go implicit in SQL (lineage
  and review require parsing transforms), a bad table name would fail at run
  time inside the build transaction instead of at resolve time, and dataset
  names would be baked into transform SQL, eroding dev/prod portability. A
  hybrid (declared norm + direct-addressing escape hatch) was also rejected:
  a partial manifest that looks complete is worse than none.
- **`query:` sources** (warehouse-dialect SQL executed server-side instead of
  `table:`). Deferred: scan-with-pushdown through the inlined view already
  covers filtering and projection, so the only thing `query:` buys is
  warehouse-native SQL (e.g. BigQuery partition decorators or dialect
  functions via `bigquery_query()`) — and warehouse-dialect SQL in
  `cell.yaml` raises portability and validation questions this ADR doesn't
  need to answer. The `Connection` source shape admits an optional `query:`
  later without breaking anything, if scan-with-pushdown proves insufficient
  for a real table. (That condition was met — a 2.57B-row view whose
  consumer needs an aggregate — and this deferral is reopened and decided
  by ADR 0007.)
- **Extract-to-Parquet as the core mechanism** (engine runs a warehouse
  export to the bucket, then reads it as Raw). Rejected: it needs a staging
  location, doubles storage, and re-implements what the scanner extensions
  already do — read directly, with pushdown.
- **A `dyn Connector` trait behind cargo features** (mirroring
  `DeployTarget`). Rejected as over-engineering *for now*: connectors add no
  Rust dependencies (the weight lives in DuckDB extensions), so there is
  nothing for a feature gate to keep off the build. The enum-plus-match seam
  carries the same additivity; a trait can be extracted if a connector ever
  brings real dependency weight.
- **A generic ADBC/Arrow Flight connector** as the one connector to rule
  them all. Deferred: auth models and driver maturity vary too much across
  warehouses today. The contract accommodates it later as simply another
  `type:`.
- **Folding `s3:` into `connections:`.** Deferred: `s3:` configures the
  storage plane (lake `DATA_PATH` + raw URIs), is shared across sources
  rather than referenced by one, and migrating it is a rename with breakage
  cost and no new capability. Revisit if per-source object-store credentials
  ever become a requirement.
