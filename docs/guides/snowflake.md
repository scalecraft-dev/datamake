# Snowflake setup

`type: snowflake` reads tables, views, and server-side SQL through the
DuckDB `snowflake` community extension, which needs the Arrow ADBC
Snowflake driver installed separately.

## Connection

```yaml
# profiles/prod.yaml
connections:
  wh:
    type: snowflake
    account: MYORG-ACCOUNT123
    user: DATAMK_SVC
    private_key_path: /etc/datamk/sf-key.p8
    database: ANALYTICS
    # warehouse: REPORTING_WH
    # role: REPORTING_ROLE
    # private_key_passphrase: ${SF_KEY_PASSPHRASE}
```

```yaml
# profiles/local.yaml — SSO through the browser
connections:
  wh:
    type: snowflake
    account: MYORG-ACCOUNT123
    user: you@example.com
    authenticator: externalbrowser
    database: ANALYTICS_DEV
```

| Field | Required | Notes |
| --- | --- | --- |
| `account` | yes | account identifier, not a URL |
| `user` | yes | |
| `database` | yes | one connection is one database |
| `private_key_path` | one of | PKCS#8 key file; relative paths resolve against the cell dir |
| `authenticator: externalbrowser` | one of | opens a browser per `run`; needs a SAML IdP on the account; refused by deploy pre-flight |
| `private_key_passphrase` | no | `${VAR}` only, for encrypted keys |
| `warehouse` | no | user's default |
| `role` | no | user's default |

There is no `password:` field.

## Key pair

```bash
openssl genrsa 2048 | openssl pkcs8 -topk8 -inform PEM -out sf-key.p8 -nocrypt
openssl rsa -in sf-key.p8 -pubout -out sf-key.pub
```

```sql
ALTER USER DATAMK_SVC SET RSA_PUBLIC_KEY='MIIBIjANBgkq...';   -- PEM body, no header/footer/newlines
```

## ADBC driver

```bash
curl -sSL https://raw.githubusercontent.com/iqea-ai/duckdb-snowflake/main/scripts/install-adbc-driver.sh | sh
```

Manual: download `snowflake_<platform>_v1.11.0.tar.gz` from
<https://github.com/adbc-drivers/snowflake/releases/tag/go%2Fv1.11.0> and
place the library at
`~/.duckdb/extensions/v1.5.4/<platform>/libadbc_driver_snowflake.so`
(the `.so` name on every platform, rename the macOS `.dylib`), or set
`SNOWFLAKE_ADBC_DRIVER_PATH`. Deployed images must bake the driver with a
pinned version and checksum. See the Dockerfile.

## Reads

- `table:` paths are `schema.table`, folded to UPPERCASE. Quoted lower/mixed-case objects are reachable only through `query:` with quoted identifiers.
- Every `table:` source is staged in full once per run. Transform `WHERE` clauses do not push down. Bound large sources with `incremental:` or `query:`.
- `incremental:` runs the watermark predicate server-side. Cursor names fold to UPPERCASE. `NUMBER(38,0)` works as an integer cursor if values fit 64 bits. For `TIMESTAMP_NTZ` cursors keep the session timezone consistent across runs.
- `query:` runs verbatim via `snowflake_query()`. Unqualified names resolve against the connection's `database`. `${connection.*}` is rejected. No dry-run preflight.
- No `staging_uri:`, no `billing_project`, no bytes-scanned field for `query:`.

## Errors

| Error | Check |
| --- | --- |
| `the Snowflake ADBC driver … is not installed` | ADBC driver section above |
| `no active warehouse` | set `warehouse:` or grant usage on the user's default |
| `table not found in database …` | the UPPERCASE path the error names; use `query:` for case-sensitive names |
| JWT/auth failure | `RSA_PUBLIC_KEY` registered on the user; key file path |
| error 390190 with `externalbrowser` | the account has no SAML IdP; use key-pair auth |

Design rationale: [ADR 0009](../adr/0009-snowflake-connector.md).
