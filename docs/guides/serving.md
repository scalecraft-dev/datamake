# Serving

`datamk serve` exposes a cell's interface as REST + OpenAPI + `/context`.

```bash
datamk serve -f cell.yaml            # profile local
datamk serve -f cell.yaml -p prod    # published profile: the poller follows LATEST
datamk serve                         # datamk.yaml in cwd → project mode; else cell.yaml
```

| Flag | Default | Scope in project mode |
| --- | --- | --- |
| `--port` | 8080 | process |
| `--max-concurrency` | 64 | per mounted cell |
| `--poll-interval` | | process; one poller thread per published cell |
| `--no-data` | off | every cell; unions with per-cell `no_data: true` |
| `--drain-timeout` | 10s | process |
| `-p/--profile` | `local` | overrides the project `profile:` and every per-cell `profile:` |

## Routes

```
GET /                        {"status":"ok"} plus execution number in published mode; pre-auth
GET /openapi.json
GET /context                 ?include=docs inlines docs pages
GET /<name>@<major>          ?<grain col>=<value>&limit=100&offset=0
```

Every route except `/` sits behind `access:`. Data responses carry
`Link: </context>; rel="describedby"`, `X-Datamk-Context-Digest`, and, in
published mode, `X-Datamk-Execution`. `/context` and `/openapi.json` send
`Cache-Control: private`.

## Query grammar

- **Grain columns**: exact equality only. No ranges, operators, or non-grain columns.
- **`limit`**: default 100, max 1000 (clamped).
- **`offset`**: max 1,000,000 (rejected above).
- Anything else is **400**. Unknown parameters are never ignored.
- Pages are ordered by the declared grain (`ORDER BY ALL` for grainless exports) before `LIMIT`/`OFFSET`.
- No SQL, filter expressions, projections, or `order_by` on this socket. Use `datamk attach` for that.
- There is no NULL literal. Rows with a NULL grain value are reachable only unfiltered; `verify` reports them as `null_rows`. Coalesce to a sentinel in the transform if callers need them.

`/context` accepts only `include=docs`. Any other parameter, unknown token,
or empty value is 400.

## Status codes

| Code | Meaning |
| --- | --- |
| 200 | Rows as a bare JSON array. |
| 400 | Unknown or invalid query parameter. |
| 401 | Missing or unknown bearer token (`access.roles` set). |
| 403 | Cell not shareable, or token lacks an allowed role. |
| 404 | No such export route. |
| 500 | Query execution failed. |
| 503 | Over the concurrency cap. Retry with backoff. |

## Multi-cell projects

A root `datamk.yaml` mounts several cells behind one port.

```yaml
datamk: 1
profile: prod                    # default for every cell
cells:
  - datamk-examples/weather      # mounts at its cell: name
  - path: dplat-datamake/flight-spend
    profile: local
    mount: flights               # URL segment
    no_data: true
```

```
GET /                        {"status":"ok","cells":["weather","flights"]}
GET /weather/                {"cell":"weather","status":"ok","execution":7}
GET /weather/context
GET /weather/openapi.json    servers: [{"url":"/weather"}]
GET /weather/temp_daily@1?city=SEA
```

- Single-cell serving is unchanged: flat routes, no `servers` block, same digest.
- Each cell keeps its own connection, catalog, poller, `principals:` file, policy, and concurrency cap. A token for one cell is 401 at another.
- `GET /` lists only the mounts whose policy admits the caller. No root `/context` or aggregate `/openapi.json`.
- Every listed cell must open or the server does not start.
- `DATAMK_MEMORY_LIMIT` is the whole process budget, divided across cells. Unparseable values fail at startup.
- `datamk deploy` does not read `datamk.yaml`. Project mode is for local, single-VM, and behind-your-own-proxy serving.
- A `mount:` override desynchronizes `mesh emit --store --url-template`.

## Operations

- **Memory**: under a cgroup, DuckDB defaults to 75% of `memory.max`. `DATAMK_MEMORY_LIMIT` overrides.
- **Shutdown**: `SIGTERM`/`SIGINT` stop accepting, drain up to `--drain-timeout`, exit 0. Run as PID 1. Keep the drain under the orchestrator's grace period (Kubernetes default 30s).
- **Throttling**: the cap is per cell, not per client. Put a reverse proxy in front for per-client limits, TLS, and request logs:

```nginx
limit_req_zone $binary_remote_addr zone=datamk:10m rate=20r/s;
server {
    listen 443 ssl;
    location / {
        limit_req zone=datamk burst=40 nodelay;
        proxy_pass http://127.0.0.1:8080;
    }
}
```

Design rationale: [ADR 0013](../adr/0013-long-form-docs-pages.md),
[ADR 0014](../adr/0014-multi-cell-serving.md).
