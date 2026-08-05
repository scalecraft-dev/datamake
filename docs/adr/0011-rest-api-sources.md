# ADR 0011 — REST API sources

- **Status:** On-hold, not implemented
- **Date:** 2026-07-31
- **Deciders:** Datamake team
- **Author:** @scottypate

## Scope

A generic, declarative REST API source: `endpoint:` joins `table:`/`query:` as
the third member of a connection source's exactly-one-of set, backed by a
`type: rest` connection in the profile. The engine owns the fetch loop —
pagination, retry, rate-limit handling, incremental cursoring — and lands
responses through the existing staged-read path into DuckLake. No Python, no
sidecar, no per-vendor connectors.

The design is informed by a review of dlt's `rest_api` source (the strongest
prior art) and its production failure modes, each of which this design makes
unrepresentable rather than merely unlikely: paginator auto-detection that
guesses wrong silently (dlt-hub/dlt#2505), client-level paginator config
silently not inherited by child resources (#2586), extract-phase memory
blowups from whole-response buffering (#2221), a timestamp-cursor race that
silently drops rows committed late with earlier timestamps (#2269), and
pipeline state split between a local `.dlt/` folder and `_dlt_*` destination
tables, invisible to any outer snapshot system.

**Decision history, owned openly.** The first team review verdicted "don't
build — document a dlt recipe" on the premise that datamk's user already has
extraction solved by a central platform (VISION.md's embedded-engineer
audience). That premise failed against the first live use case (Salesforce
accounts, no warehouse copy, no EL vendor, and no appetite to buy one): for
any team whose escape from the platform tax is the point, extraction is
unsolved by definition. A per-vendor community-extension connector
(`duckdb-salesforce`, per ADR 0009's snowflake precedent) was considered and
rejected — it starts a connector-catalog treadmill against incumbents with
hundreds, and stakes a roadmap item on a v0.14 single-maintainer extension.
One closed framework that covers Salesforce, HubSpot, and the next API is the
durable position. The dlt → Parquet → raw-source recipe remains the
documented interim bridge while this ships, and nothing more.

## Decision

### 1. Config shape: a third connection-source target, not a new source kind

`Source` keeps its three-way dispatch (string / `cell` / `connection`,
`src/config/schema.rs:249`). An API source is a **connection source** whose
object reference is `endpoint:` — exactly-one-of `table`/`query`/`endpoint`,
enforced in `deserialize_connection` alongside the existing pair (ADR 0007
§1). The profile grows `type: rest` beside `bigquery`/`postgres`/`snowflake`
(`schema.rs:827`), reusing the connection binding, resolve-time errors, and
secret-resolution machinery whole.

The contract/environment split, ruled by "properties of the API are contract;
properties of your tenancy are environment":

```yaml
# profiles/prod.yaml — ENVIRONMENT
connections:
  hubspot:
    type: rest
    base_url: https://api.hubapi.com
    auth: { kind: bearer, token: "${HUBSPOT_TOKEN}" }
    rate_limit: 100/s          # your plan tier
    timeout: 30s
```

```yaml
# cell.yaml — CONTRACT
sources:
  raw_contacts:
    connection: hubspot
    endpoint: /crm/v3/objects/contacts
    params: { properties: "email,lifecyclestage", limit: 100 }
    records: results           # required — no guessing results/data/items
    paginate:
      kind: cursor
      cursor_path: paging.next.after
      param: after
    columns:                   # required — declared, never inferred (§3)
      id: bigint
      email: string
      lifecyclestage: string
      updatedAt: timestamp
```

**Blocking implementation detail:** three error strings currently enumerate
`table`/`query` as the closed set (`schema.rs:329`, `schema.rs:343-360`).
Shipping `endpoint:` without rewriting all three ships a lie. `bindings.rs`
must likewise reject `endpoint:` against a non-rest connection (and
`table:`/`query:` against a rest one) with both the source name and the
connection name in the text.

### 2. Pagination: a closed enum, required, endpoint-local

`paginate.kind` is one of exactly five: `none`, `next_url` (absolute next
link in the body), `cursor` (token in the body → request param), `offset`
(offset/limit params), `page` (page-number param). Each kind's fields are
`deny_unknown_fields`; the enum is closed per ADR 0008's admission standard —
a bounded set with zero query semantics, not a plugin surface.

- **No auto-detection.** A wrong guess yields page 1 and exit code 0 — silent
  and unfalsifiable, the failure mode this codebase already refuses
  (`MaterializeStrategy`'s closed set, `Incremental`'s deny-unknown-fields).
  `paginate:` is required; `kind: none` is a declaration, not a default.
- **No connection-level paginator.** Pagination is declared on the endpoint
  or nowhere; dlt's inheritance bug (#2586) has no representation here.
- **Stall detection.** A page returning the same cursor/URL as its
  predecessor aborts the run with both values in the error, rather than
  looping or silently stopping.

### 3. Schema: declared, never inferred

`columns:` is required and passed to `read_json(columns => {...})` with
sampling off and `union_by_name` false. A new field the API starts sending is
a `cell.yaml` edit or it does not exist — schema drift reaches the interface
only through the contract, per the same doctrine `verify` enforces on the way
out. An opt-in `raw: true` adds a `_raw JSON` column carrying the full record
for forensics and later re-modeling. `records:` (the array's path in the
response body) is likewise required; a miss aborts naming the actual
top-level keys of the response, because a silent zero-row extract is the
failure users take longest to notice.

### 4. Staging and atomicity: fetch pre-`BEGIN`, stream to scratch

The fetch loop runs **before** the transform transaction, writing records as
NDJSON into the cell's scratch directory (already RAII-dropped,
`src/engine/mod.rs:65`), then `CREATE TEMP TABLE … AS SELECT * FROM
read_json(…)` → TEMP VIEW — `stage_via_export`'s shape
(`src/engine/mod.rs:1881`) with HTTP in place of `EXPORT DATA`. The invariant
at `engine/mod.rs:2453` (staging statements read-only against `lake`,
pre-`BEGIN`) carries over unchanged. Consequences:

- A mid-pagination failure — 401 at page 5, a 429 storm, a stall — aborts
  before `BEGIN`. **A partial extract cannot commit, by construction.** The
  error narrates pages and rows fetched so the author knows what was
  discarded.
- Responses stream to disk (`bytes_stream()` → chunked NDJSON writes); no
  code path materializes a whole response body in memory (dlt #2221's
  blowup). This holds under chunked and gzip transfer-encoding, verified by
  test, not by review.
- A failed page is retried as the **same request**; pagination state advances
  only on a 2xx that parsed. Retry can therefore never skip or duplicate a
  page.
- Retry/backoff honors `Retry-After` on 429 and applies capped exponential
  backoff on 5xx/transport errors; the profile's `rate_limit` throttles
  proactively.

### 5. Incremental: ADR 0005's watermarks, with the timestamp race closed

Cursor state lives in `__datamk_watermarks` (`src/engine/mod.rs:2074-2360`),
keyed by source, persisted in the same transaction as the data
(`persist_watermarks`, `mod.rs:2626`) — one state owner, rollback-coherent,
exactly what dlt's split-brain state is not. `incremental:` reuses ADR 0005's
`cursor` and `lookback` verbatim and adds three API-only fields, rejected at
resolve time on warehouse sources:

- `type` — the cursor's encoding in the **response** (`timestamp_iso8601`,
  `timestamp_ms`, `integer`, `opaque`). Required: there is no catalog to
  bind-check against, so ADR 0005's `DESCRIBE`-time validation has no analog
  and the declaration replaces it. A first-record value that contradicts the
  declared type aborts, naming the value and the available fields.
- `param` — the request query parameter carrying the watermark.
- `format` — the watermark's encoding in the **request**, which may differ
  from `type`. Required, never defaulted: this is dlt's documented
  type-mismatch footgun.

**The #2269 race is closed by construction, not by care:** for timestamp
cursors, `lookback` is **required** (unlike ADR 0005's optional), and the
watermark advances to `min(max_seen, request_start − lookback)` — a row
committed mid-extract with an earlier timestamp is inside the next run's
overlap window by arithmetic. Overlap re-delivery is only safe under
idempotent apply, so an incremental endpoint source requires a
`materialize: upsert` transform downstream — enforced by the same gate as
§6. Opaque cursors (page tokens) skip `lookback`; they are replayed from the
provider's semantics, not time arithmetic.

`--verify-replay` replays from the retained scratch NDJSON, never the
network: replay must not depend on a rate-limited third party, and the API
would not return the same answer twice anyway.

### 6. The match-site audit is part of the change, not a follow-up

Three sites currently pattern-match `Source::Connection` /
`ResolvedSource::Connection` and would silently exempt endpoint sources from
replay safety:

- `verify::incremental_source_names` (`src/verify.rs:183`) — an incremental
  endpoint source feeding a `materialize: replace` transform would bypass
  ADR 0008's truncation gate entirely: the founding incident, reintroduced.
  **Critical.**
- `run`'s `incremental_count` (`src/engine/mod.rs:544`) — `--full-refresh`
  narration, the shrink detector, and `--verify-replay` all silently no-op.
- `resolve_incremental` (`src/config/bindings.rs:348`) — must admit the
  endpoint variant with the API-only fields and refuse them elsewhere.

Acceptance for the incremental PR is a test per site proving an endpoint
source is governed identically to a warehouse source.

### 7. Auth: static credentials and `client_credentials` — no refresh tokens

`auth.kind` is one of: `bearer` (static token), `header` (named header),
`query` (named query param — some APIs insist), `client_credentials` (OAuth2
server-to-server: token endpoint, id, secret; tokens acquired per run, held
in memory, re-acquired on 401-once-then-abort). Salesforce's Connected Apps
support `client_credentials`, so the first live use case is in scope.

**Refresh-token flows are refused in v1**, not deferred silently: a rotating
refresh token is mutable state that must land either in the catalog artifact
— published and rolled back *with the data*, so a rollback resurrects a
revoked token — or in a hidden sidecar file, dlt's `.dlt/` mistake. Neither
is acceptable; the config error says so and names `client_credentials` as
the supported alternative.

Secret-valued fields follow the whole-value `${VAR}` fail-loud-on-empty rule
(`src/config/connections/snowflake.rs:92-116`). Tokens live in Rust memory
for the HTTP client only — never interpolated into SQL, never logged, never
in a DuckDB secret (there is no `ATTACH`; nothing needs one).

### 8. HTTP core: `reqwest`, blocking, fenced off from `serve`

`reqwest` is already in the dependency graph transitively (via `aws-config`);
declaring it adds a manifest line, not a runtime. The fetch loop uses the
blocking client on the sync `engine::run` path — matching the connector
modules' style and avoiding `Handle::block_on` on the tokio main thread (the
panic documented at `src/store.rs:124`). The module boundary, not
convention, keeps it unreachable from `serve`'s async handlers: the fetch
module is `pub(crate)` to the engine and takes types `serve` never holds.

## Alternatives considered

- **dlt, loosely coupled** (user-run pipeline → Parquet → raw source): works
  today, zero datamk changes, and remains the documented interim. Rejected as
  the *answer* because it outsources a core workflow to a Python sidecar the
  single-binary story exists to eliminate, and its failure modes (silent
  merge-to-append degradation on filesystem destinations, split state) are
  the ones this ADR is designed against.
- **dlt orchestrated as a subprocess:** the Python tax *and* the dual-state
  tax, to save the user one cron entry. The worst point on the curve.
- **Per-vendor community extensions** (`duckdb-salesforce` per the ADR 0009
  pattern): rejected above. Revisitable per vendor if an extension reaches
  the maturity bar ADR 0009's snowflake extension met — a connector ADR each,
  against ADR 0003's contract, competing on merit with this framework's
  coverage of the same API.

## Consequences and risks

- **`verify` cannot see missing data** — a schema-valid, grain-unique
  snapshot built from fewer rows than the API holds passes. ADR 0007 already
  names this hazard class for `query:` sources. Mitigations, in order of
  force: any non-2xx or stall aborts the whole run pre-`BEGIN` (§4) — the
  silent case is reduced to "the API itself returned wrong data with 200s";
  the run log narrates pages/rows per endpoint source; the shrink detector
  (§6) flags row-count regressions run-over-run; ADR 0007's author-owned
  gated correctness test is the recommended pattern for cells where an
  undetected shortfall over-invoices someone.
- **Streaming correctness under chunked/gzip is the highest-risk code** and
  gets its own review pass and tests against a local mock server (which also
  serves the paginator and retry tests — no live API in CI).
- **Five PRs:** config (§1), fetch core (§4, §8), paginators (§2), 
  incremental + match-site audit (§5, §6), auth (§7). Roughly two weeks.
  A mid-pagination 429 and a `--verify-replay` run are the two adversarial
  tests the last PR must include.

## Premises

This decision holds while: (a) users exist whose blocker is landing API data,
not modeling it — established by the first live use case, falsified if that
class of user turns out empty; (b) the target APIs are reachable with §7's
auth set — falsified for Salesforce if the org's admin policy disables
`client_credentials` flows (check the Connected App settings before the auth
PR; if disabled, the v1 scope question reopens, not the whole ADR).
