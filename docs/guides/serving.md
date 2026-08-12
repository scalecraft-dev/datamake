# Serving

`datamk serve` exposes a cell's declared interface as REST + OpenAPI. This
guide covers the operational surface of that endpoint: the query grammar it
accepts, how it behaves under load, and what to put in front of it.

## Serving several cells from one process (ADR 0014)

A root `datamk.yaml` lists the cells one process mounts behind one port:

```yaml
# datamk.yaml — the cells this project serves behind one port.
# Paths are relative to this file; a directory means <dir>/cell.yaml.
# Only `datamk serve` reads this file today.
datamk: 1

# The profile every cell uses unless it names its own.
# `datamk serve -p <name>` overrides this AND every per-cell `profile:`.
profile: prod

cells:
  # Shorthand: just the path. The cell mounts at its own declared name.
  - datamk-examples/weather

  # Long form: the same path, plus per-cell overrides.
  - path: dplat-datamake/flight-spend
    profile: local
    mount: flights     # URL segment; defaults to the cell's `cell:` name
    no_data: true      # serve /flights/context, mount no data routes
```

`datamk serve` with no `-f` uses `datamk.yaml` if it's in the current
directory, else `cell.yaml`. Each cell keeps its own everything — connection,
catalog, poller, principals file, authorization policy, and concurrency cap.
The process is shared; nothing else is.

```
GET /                       {"status":"ok","cells":["weather","flights"]}
GET /weather/               {"cell":"weather","status":"ok","execution":7}
GET /weather/context
GET /weather/openapi.json   servers: [{"url":"/weather"}]
GET /weather/temp_daily@1?city=SEA&limit=50
```

A cell's base URL is `https://host/weather`, so a mesh manifest entry is just
`{"name": "weather", "url": "https://host/weather"}` — `mesh emit` needs no
special handling. Note that a `mount:` override desynchronizes
`mesh emit --store --url-template "https://host/{name}"`, which derives names
from store prefixes rather than from your project file.

**Serving one cell is unchanged.** Flat `/`, `/context`, `/<name>@<major>`,
no `servers` block, same headers. A cell mounted in a project has the same
interface digest it has standing alone.

### What the root route does and does not say

`GET /` lists only the mounts whose own `authorize()` admits the caller — an
anonymous caller against an all-private project gets `[]`, and a
non-shareable cell is invisible to everyone. `status` is unconditional so a
liveness probe needs no credential.

It carries names only: no exports, no descriptions, no digests, and there is
no root `/context` or aggregate `/openapi.json`. A route that summarized
other cells would be a served mesh manifest, which ADR 0012 §6 refuses.
Discovery across cells is `datamk mesh emit`'s job — a static document you
host, not a live index this socket serves.

### Flags in project mode

| Flag | Scope |
| ---- | ----- |
| `--port` | Process. One socket. |
| `--max-concurrency` | **Per mounted cell.** A shared cap would let one cell's saturation shed every other cell's liveness route. |
| `--poll-interval` | Process. Each published cell gets its own poller thread, staggered. |
| `--no-data` | Applies to every cell, and unions with per-cell `no_data: true`. There is no per-cell way to turn it off. |
| `-p/--profile` | Overrides the project `profile:` **and** every per-cell `profile:`. |

Set `DATAMK_MEMORY_LIMIT` to the process's whole budget, not one cell's:
`serve` divides it across the mounted cells and caps DuckDB's thread count
per connection. An unparseable value fails at startup rather than quietly
handing every cell the full budget.

Every listed cell must open or the server does not start — a partially
mounted server passes its own probe while serving 404s a caller cannot tell
from a typo.

### What co-tenancy costs you

One process is one failure domain, one restart, one scaling unit, and one
set of resource limits. Cells with materially different sensitivity are the
case where this is the wrong packaging: a per-cell `profile:` pins that
cell's credentials *and* its `principals:` file, so a cell pinned to `local`
in a production deployment serves a local catalog under a local policy while
the process still looks healthy. The startup banner prints the profile for
every mount — read it.

`datamk deploy` does not read `datamk.yaml`; it renders a single-cell
workload. Multi-cell serving is the local, single-VM, and
behind-your-own-proxy story.

## The query grammar is closed

A data route (`GET /<name>@<major>`) accepts exactly:

- **Grain columns** as filters — exact equality only. No ranges, no
  operators, no non-grain columns.
- **`limit`** — rows per page. Values above the maximum (1000) are clamped;
  the default is 100.
- **`offset`** — rows to skip. Requests beyond the maximum (1,000,000) are
  rejected; filter by grain columns instead of paginating that deep.

Anything else — an unknown parameter name, a non-grain column, a
non-integer `limit`/`offset` — is rejected with **400**, never silently
ignored. A silently ignored filter would return unfiltered rows the caller
reads as a filtered subset: a confidently wrong number, not an error.

Pagination is deterministic: the read is ordered by the declared grain
(every column, via `ORDER BY ALL`, for a grainless export) before
`LIMIT`/`OFFSET` applies, so pages stitch together without skipping or
double-counting rows.

datamk never accepts a query from a caller — no filter expressions, no
projections, no `order_by`, on this socket, in any version. A consumer that
needs real SQL should attach the storage plane read-only (`datamk attach`)
and run it with its own engine, credentials, and bill.

## `/context`'s query grammar is closed too (ADR 0013)

`GET /context` accepts exactly one query parameter:

- **`include`** — a comma-separated list drawn from a closed vocabulary
  (today: `docs`). `?include=docs` inlines every declared docs page's
  content into the response; omit it for the default document (identity
  and measurements only, no page content).

Any other parameter name, an unrecognized `include` token, or an empty
value (`?include=` or a trailing comma) is **400** — the same
never-silently-ignored discipline as the data door. `?include=docs` on a
cell with no `docs:` fields is a normal **200** with an empty `docs: {}`,
not an error.

The two variants carry different `ETag`s (the docs variant appends a
content-hash suffix) so caching and `If-None-Match` work correctly per
variant; `X-Datamk-Context-Digest`, `/openapi.json`'s `info.version`, and
the mesh manifest's `context_digest` all stay pinned to the plain interface
digest regardless of which variant produced them. Both `/context` and
`/openapi.json` now send `Cache-Control: private` — `authorize()` is
all-or-nothing, and a shared cache keyed on URI alone could otherwise hand
a cached 200 to a caller with no token.

## Throttling

`serve` applies one concurrency cap per served cell (`--max-concurrency`,
default 64): requests over the cap are shed immediately with **503**, instead
of queueing without bound. Agents fan out and retry tirelessly; shedding keeps
the endpoint honest about capacity instead of stacking latency. In a project
the caps are independent, so a saturated cell sheds its own traffic and not
its neighbours'; the project root sits outside them entirely and stays
answerable.

The cap is per cell, not per client — one greedy caller can consume a cell's
whole allowance, and a project of N cells admits up to N times the cap in
total. For
per-client rate limiting, fairness, TLS termination, and request logging,
put a reverse proxy in front. Example (nginx):

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

On Kubernetes, the same job belongs to your ingress controller (e.g.
`nginx.ingress.kubernetes.io/limit-rps`) or service mesh.

## Status codes

| Code | Meaning |
| ---- | ------- |
| 200  | Rows (a bare JSON array). |
| 400  | Unknown or invalid query parameter. |
| 401  | Missing or unknown bearer token (`access.roles` is set). |
| 403  | Cell not shareable, or token lacks an allowed role. |
| 404  | No such export route. |
| 500  | Query execution failed. |
| 503  | Over the concurrency cap — retry with backoff. |

The pre-auth `/` health route carries one liveness fact (plus the served
execution number in published mode); everything else — `/context`,
`/openapi.json`, and the data routes — sits behind the same `access`
policy.
