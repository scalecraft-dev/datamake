# Serving

`datamk serve` exposes a cell's declared interface as REST + OpenAPI. This
guide covers the operational surface of that endpoint: the query grammar it
accepts, how it behaves under load, and what to put in front of it.

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

## Throttling

`serve` applies one global concurrency cap (`--max-concurrency`, default
64): requests over the cap are shed immediately with **503**, instead of
queueing without bound. Agents fan out and retry tirelessly; shedding keeps
the endpoint honest about capacity instead of stacking latency.

The cap is global, not per-client — one greedy caller can consume it. For
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
