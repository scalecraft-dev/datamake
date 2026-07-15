# Snowflake sources

`connection` sources can read Snowflake tables, views, and server-side SQL.
The connector contract they plug into is [ADR 0003](../adr/0003-generic-source-definitions.md);
the Snowflake-specific decisions and their trade-offs are
[ADR 0009](../adr/0009-snowflake-connector.md). This page is the operating
guide — what to write, what the engine promises, and where the sharp edges
are.

## 1. The connection

Connections are environment config and live in the binding profile:

```yaml
# profiles/prod.yaml (gitignored, like every non-local profile)
connections:
  wh:
    type: snowflake
    account: MYORG-ACCOUNT123          # account identifier — not a URL
    user: DATAMK_SVC
    private_key_path: /etc/datamk/sf-key.p8   # PKCS#8 key file (secret mount)
    database: ANALYTICS                # environment root: `table:` paths are schema.table
    # warehouse: REPORTING_WH          # omit to use the user's default warehouse
    # role: REPORTING_ROLE             # omit to use the user's default role
    # private_key_passphrase: ${SF_KEY_PASSPHRASE}   # only for encrypted keys; ${VAR} form required
```

```yaml
# profiles/local.yaml — same cell, your own SSO login instead of a key file
connections:
  wh:
    type: snowflake
    account: MYORG-ACCOUNT123
    user: you@example.com              # your Snowflake login name
    authenticator: externalbrowser     # opens your browser at the IdP login page
    database: ANALYTICS_DEV
```

- **Auth is exactly one of two mechanisms.** `private_key_path:` (key-pair —
  the service-account shape for deployed cells) or `authenticator:
  externalbrowser` (SSO through your browser — the local-dev shape). There is
  no `password:` field: no literal secret ever lives in a profile field
  (ADR 0003 §2), and Snowflake is phasing out single-factor passwords for
  service users anyway.
- `externalbrowser` is interactive by nature: each `datamk run` opens your
  browser once at attach. **Deploy pre-flight refuses it** — a Builder pod
  has no browser; deployed profiles use `private_key_path:`.
- `externalbrowser` **requires a SAML/SSO identity provider** (Okta, AzureAD,
  …) configured on the Snowflake account. An account with no IdP rejects the
  flow server-side (Snowflake error 390190) — the underlying driver stack
  only implements the SAML path (gosnowflake defaults console login off and
  the ADBC driver exposes no switch), so Snowflake's IdP-less console login
  is not reachable through datamk today. On such an account, use key-pair
  auth even locally.
- `database` is required — it is the environment root, the analog of
  BigQuery's `project`. One connection ≡ one database; a cross-database read
  is a second connection, not a three-part table name.
- Every field is `${VAR}`-expandable. `private_key_path` may be relative
  (resolved against the cell directory, like `principals:` and transforms).

### Generating a key pair

```bash
openssl genrsa 2048 | openssl pkcs8 -topk8 -inform PEM -out sf-key.p8 -nocrypt
openssl rsa -in sf-key.p8 -pubout -out sf-key.pub
```

then register the public half on the Snowflake user (strip the PEM header/
footer/newlines, or use Snowsight's user settings):

```sql
ALTER USER DATAMK_SVC SET RSA_PUBLIC_KEY='MIIBIjANBgkq...';
```

An encrypted key (`-passout` instead of `-nocrypt`) needs
`private_key_passphrase: ${SOME_VAR}` — the value must be a `${VAR}`
reference so the secret lives in the environment, never in the profile file.

## 2. The ADBC driver {#adbc-driver}

The DuckDB `snowflake` community extension reads through the **Arrow ADBC
Snowflake driver**, a separate native library datamk cannot install from
DuckDB's extension registry. Without it, the first attach fails with an
error pointing here.

One-line install (macOS arm64 / Linux; inspect it first if you prefer):

```bash
curl -sSL https://raw.githubusercontent.com/iqea-ai/duckdb-snowflake/main/scripts/install-adbc-driver.sh | sh
```

or manually: download `snowflake_<platform>_v1.11.0.tar.gz` from
<https://github.com/adbc-drivers/snowflake/releases/tag/go%2Fv1.11.0>,
extract, and place the library at
`~/.duckdb/extensions/v1.5.4/<platform>/libadbc_driver_snowflake.so`
(the `.so` name is expected on every platform — rename the macOS `.dylib`),
or set `SNOWFLAKE_ADBC_DRIVER_PATH` to wherever you put it.

**Deployed images must bake the driver** (and pin its version + checksum),
exactly as connector extensions are baked per ADR 0003 §4 — a credential-
handling native binary must never be pulled from a release page at pod
start. See the Dockerfile.

## 3. `table:` sources

```yaml
# cell.yaml — contract, env-free as always
sources:
  hourly_production:
    connection: wh
    table: staging.hourly_production     # schema.table — the database comes from the connection
```

- Table paths are exactly `schema.table`. datamk resolves them with
  **Snowflake's own unquoted-identifier rule: folded to UPPERCASE** —
  `staging.hourly_production` reads `STAGING.HOURLY_PRODUCTION`. A genuinely
  lower/mixed-case object (one created with quoted identifiers, e.g.
  `"sqlmesh"."_versions"`) is not reachable via `table:`; read it with a
  `query:` source using quoted identifiers.
- Base tables **and views** both work — there is no BigQuery-style
  storage-vs-jobs split.

**The honest performance note:** every Snowflake `table:` source is staged —
materialized into a session-local temp table — **in full, once per run**,
plain base tables included. Transform `WHERE` clauses do **not** push down to
Snowflake (the opposite of BigQuery base tables, which bind read-through
with pushdown; the Snowflake extension's scan cannot survive arbitrary
transform SQL, so datamk never exposes it to transforms). Staging once per
run also means N transforms referencing a source cost one Snowflake read,
not N. To bound a large source, use `incremental:` (§4) or aggregate
server-side with `query:` (§5) — the run log says exactly this when a
non-incremental source is staged.

## 4. `incremental:`

The [incremental guide](incremental.md) applies unchanged:

```yaml
sources:
  events:
    connection: wh
    table: raw.events
    incremental:
      cursor: ingested_at
      lookback: 2h
```

The watermark predicate is pushed down into the Snowflake read (verified
byte-correct for `DATE` and `TIMESTAMP_TZ` cursors), so steady-state runs
transfer only the delta. Snowflake's `NUMBER(38,0)` integer columns work as
cursors (they surface as `DECIMAL(38,0)`; values must fit in 64 bits).

`TIMESTAMP_NTZ`/`TIMESTAMP_LTZ` cursors work through the same DuckDB-side
comparison; if your cursor is `TIMESTAMP_NTZ`, keep the session timezone
consistent across runs (datamk compares against the watermark in the DuckDB
session, and the ADBC driver surfaces NTZ values as naive timestamps).

## 5. `query:` sources

```yaml
sources:
  spend_by_model:
    connection: wh
    query: |
      SELECT model_id, SUM(msrp) AS total_msrp
      FROM raw.vehicle_models
      GROUP BY model_id
```

Server-side Snowflake SQL (ADR 0007), executed verbatim — this is the lever
for pushing aggregation/projection into the warehouse when `table:`'s
full-read staging would drag too much across the wire, and the only route to
case-sensitive (quoted) objects. Unqualified `schema.table` names resolve
against the connection's `database` (the session database), so no
`${connection.*}` placeholder is needed — `${connection.project}` is a
BigQuery binding and is rejected here.

Unlike BigQuery there is **no free dry-run**: a broken query fails loud at
the real read, and there is no bytes-scanned preflight narration. Guard
spend with your own `EXPLAIN` and the warehouse's resource monitors.

## 6. Failure shapes worth knowing

- **"the Snowflake ADBC driver … is not installed"** — §2.
- **"no active warehouse"** — the connection sets no `warehouse:` and the
  user has no default. Set `warehouse:` on the connection (or grant usage on
  the one it names).
- **"table not found in database …"** — the error names the UPPERCASE-folded
  path datamk actually looked for; check it exists and that the connection's
  role can see it, or use `query:` for a case-sensitive name.
- **JWT/auth failures at attach** — the public key isn't registered on the
  user (`ALTER USER … SET RSA_PUBLIC_KEY`), or the key file is wrong. datamk
  verifies the key *file exists* before attaching; Snowflake verifies the
  rest.

## 7. What Snowflake sources don't have

No `staging_uri:` and no oversized-result escape hatch — results stream over
Arrow with no BigQuery-style ~10GB response ceiling. No `billing_project`
analog — compute cost lands on the connection's `warehouse`. And no
bytes-scanned run-summary field for `query:` sources (§5).
