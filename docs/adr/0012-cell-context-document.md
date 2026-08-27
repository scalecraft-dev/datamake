# ADR 0012 — The context document: cells describe themselves to agents

- **Status:** Accepted — 2026-08-05, premise checks run (results under Premises).
  Implemented — 2026-08-06, all five PRs on `adr-0012-context-document`
  (prerequisites; document + `/context` + `datamk context`; meaning fields +
  ratchet; probes + `--no-data`; mesh emission).
- **Date:** 2026-08-04
- **Deciders:** Datamake team
- **Author:** @scottypate

## Motivation

The live use case: a company with a large, complex estate of metrics wants to
point its own Claude instances at its data and have them self-orient — what
products exist, what the columns mean, whether the numbers can be trusted —
without a human explaining the landscape first.

Today that agent gets `/openapi.json`: column names, coarse JSON types, a
hardcoded `version: "0.0.0"`, and no meaning. Walked concretely against the
`flight-spend` design-partner cell, an agent must guess whether invoiced
revenue is `invoice_amount` or `uncapped_spend`, what `flight_id = -1` means,
and whether `month` is UTC — and each wrong guess produces a confidently wrong
number, silently. Meanwhile the author of that cell already wrote every one of
those answers into `cell.yaml` — as YAML comments that serde discards.

The frame that resolves this: **a cell is the interface layer for a data
product.** The estates cells draw from have no interface — a warehouse is a
flat pile of tables with grants: no versioning, no declared grain, no meaning,
no boundary between scratch and contract. A cell is that boundary for the
product it exports: a versioned, verified, documented surface with everything
else private behind it. (Fronting or replacing the warehouse itself is a
stated non-goal, `VISION.md:112-113` — the boundary belongs to the product,
not the platform.) The context document is the boundary
made machine-readable — the same relationship `/openapi.json` has to an API.

## Decision

### 1. Context is a projection of the cell, not a separate product

There is one artifact — the cell — and the context document is a standard
projection of its interface, available for every cell with no separate thing
to adopt or maintain. Write the cell, the context exists; build the cell, the
context becomes trustworthy. Its lifecycle:

- **draft** — the cell has never been built. Every field is a declaration.
- **verified** — a build ran and `verify` passed; the document carries real
  provenance (execution, snapshot pin, verify outcome).

The build is the non-optional half of this. What separates the context
document from a semantic layer is that the connection to real data is
load-bearing: `verify` runs against the actual rows (column existence and
grain uniqueness today; declared types once §7's promotion lands), and that
build is what entitles the document to say anything at all. The *door* is
the configurable half — served rows over HTTP, `datamk attach`, or the
operator's existing warehouse grants scoped to exported objects — an
enforcement mode chosen per estate. Where the HTTP door is mounted, the two
routes complete each other rather than sitting side by side: `/context`
hands the agent its first working call (§2's `sample_request`, §5's
`example_request`), the data route's every response points back at
`/context` (§4), and ten fetched rows ground the prose against the thing
itself. The served sample is *contract-shaped* — `/:route` projects exactly
the declared schema columns — which an agent's own `SELECT *` against the
storage plane is not.

This tightens the "definitions-only" case rather than dissolving it: a
customer who wants to ship meaning without serving rows (regulated/PII
estates where agents may know what `total_comp` means but never see rows)
runs a **verified** cell with the data routes unmounted — the build and
`verify` still ran against the real connection; only the door is closed.
Draft is the inner loop before the first build, not a shipping mode:
`datamk context` emits a draft to stdout so an author can see what an agent
will see, but a hosted `/context` is never draft — `serve` requires a
published execution, and a never-built document served over HTTP is exactly
the standalone dictionary the Alternatives section refuses: permanently
hosted unverified prose wearing our route name.

### 2. Document shape

- A typed Rust struct with `Serialize` — never inline `json!` — with the
  stable-field-names contract comment and a golden serialization test, per the
  `RunSummary` precedent (`src/engine/run_summary.rs:13-14`, `:95`).
- **`datamk_context: 1`** — an integer document-schema version, distinct
  from cell semver and `datamk_version`. Additive changes don't bump; any
  removal, rename, or re-meaning bumps, with the prior version served through
  a deprecation window (the same side-by-side rule exports get).
- **`declared` vs `observed` are structurally separate regions and are never
  flattened.** `declared` holds author claims (descriptions, freshness
  claims); `observed` holds machine facts (verify outcome, snapshot pin,
  build timestamps, coverage). This is the single most important shape
  constraint: an agent must never be able to mistake a claim for a
  measurement.
- **Absent facts are omitted or `null`, never fabricated.** Direct-attach
  mode writes no `RunSummary` (`src/engine/run_summary.rs:6-8`); such cells
  serve `provenance: null`, not zeros.
- **Unbuilt cells assert their status positively**: top-level
  `status: "draft"`, `observed: null` (absent, never `{}`),
  `grain_verified: false` (never `null`, never `true`), and an engine-emitted
  note: "Nothing behind this document has been built or verified." Absence
  alone reads to an agent as "couldn't compute," not "never existed."
- **`data: { served_here: <bool>, channels: [...] }`** — `served_here` is
  derived from whether data routes are mounted (honest by construction);
  `channels` (where rows actually live when not served here) is environment
  and binds in the profile, never in `cell.yaml`.
- **A `query` block per export states the query grammar exactly**: `filters`
  (the grain columns), `filter_semantics` ("exact equality only — no ranges,
  no operators, no non-grain columns"), `limit` default/max, `offset`. This
  is the line that stops an agent inventing `?order_date__gte=`. It also
  carries **`sample_request`** — the smallest legal call, e.g.
  `/flight_spend@1?limit=10`, a pure function of the route key and the limit
  defaults. The `query` block is the grammar; `sample_request` is one
  grounded sentence in it, and agents copy sentences. Its prose claims
  (`filter_semantics`, the limit caps) restate what `build_query` enforces
  (`src/serve/mod.rs:18-19`, `:361-386`); a fixture test binds the two so a
  change to either fails loudly (§7). **Unconditional** — present for every
  export whose table is not `materialize: never` (that nulls it: no query
  affordance exists for a virtual export, ever). *Not* gated on `--no-data`
  (amended; see "Amendment" below) — it is interface grammar, not a claim
  about whether this surface currently mounts the route, and `served_here`
  already carries that claim honestly.
- **`notes[]` is engine-emitted only.** No author-supplied strings land
  there, ever — author prose lives in `declared`, where it is labeled as a
  claim.
- The document digest becomes the `ETag` and replaces `/openapi.json`'s
  hardcoded `info.version: "0.0.0"` (`src/serve/openapi.rs:21`). Not the
  execution number — that changes on data refreshes without the interface
  changing. For the same reason the digest covers the interface regions —
  shape, `declared`, `query` — and never `observed` telemetry.

### 3. Meaning fields — the only new authoring surface

`ARCHITECTURE.md:102-103` already declares the interface's job as "exported
objects, their schema, their grain, **their meaning**, and freshness
expectations." Meaning is in the doctrine; `Export`
(`src/config/schema.rs:40-60`) never grew a field for it. This closes that
gap. The anti-rot rule (`ARCHITECTURE.md:171-173`) forbids *decoupled*
catalogs; prose that lives beside the schema it describes, versions with it,
and ships in the same artifact has no independent lifecycle to rot on.

Exactly four fields, admitted under the rationing rule *(not machine-derivable
AND wrongness produces a confidently wrong number, not an error)*:

- `cell.description` — one line; also fixes `openapi.info.description`.
- `export.description` — one or two sentences: what one row means.
- per-column `description` — non-obvious columns only.
- per-column `unit` — structured (`USD`), not prose; the #1 silent-wrong-number
  source.

Schema shape: the `schema:` map value becomes *either* the bare type string
(today, unchanged — every existing cell parses as-is) *or* a mapping
`{type, unit, description}`. Hand-rolled `Deserialize` dispatching on YAML
shape per the `Source` precedent (`src/config/schema.rs:249-298`) — not
`#[serde(untagged)]`, which swallows field errors. `deny_unknown_fields`;
prose length-capped at parse time.

**The ratchet (ships with the fields, or the fields don't ship):**

1. Prose lives on the `Export` and nowhere else — no side files, no
   annotation store, no out-of-band metadata writes.
2. A description keyed to a column absent from `schema` is a hard `verify`
   failure (rename/drop kills the orphaned sentence).
3. `published.json` carries a per-route description digest; a changed
   description without a version bump is at minimum a warning — a change in
   meaning is MAJOR per `ARCHITECTURE.md:141-143`, and silent meaning-edits
   are the exact betrayal that section names.
4. `verify` lint: an export with `contract: supported` must carry a non-empty
   description. Experimental exports need nothing. Friction lands exactly on
   the deliberate promotion gesture. `verify` also refuses
   `contract: supported` on any export with no verified build — draft can
   never wear the verified costume.

**Refused:** `owner:` (environment — belongs to the deploy plane), `tags:`
(free taxonomy, catalog rot in miniature), and a `metrics:` DSL. **A metric
is an export** — executed, grain-checked, versioned, snapshot-pinned — and
what agents need is to find the export that computes it and know what it
means. A block of SQL expressions nothing ever executes is the dbt-metrics
graveyard and the same treadmill shape ADR 0011 rejected for connectors.

### 4. Surfaces

- **`GET /context`** — the route name matches the concept name. Today's
  `/interface` (already behind `authorize()`, `src/serve/mod.rs:273`) is a
  stub (route keys + freshness only) with no consumers scripted against it;
  it is renamed, not duplicated — this ADR is the moment the document
  becomes a contract, and a rename is free now and a break forever after.
  `/interface` goes away in the same release (404 — same rule as unmounted
  data routes: no door, no 403). There is still exactly one document; we do
  not own drift between two routes describing the same exports. No
  reserved-name collision exists: export routes always carry the major
  (`name@major`, `src/config/schema.rs:78-80`), so an export named
  `context` serves at `/context@1` and can never shadow `/context`.
- **The data route answers back.** Every `/:route` response — 200 and 404
  alike, because a wrong guess is exactly when the map matters — carries
  `Link: </context>; rel="describedby"` and `X-Datamk-Context-Digest` (the
  interface digest, so an agent detects mid-session that its cached context
  went stale). Not `ETag` — on a data response that tags the rows, and
  overloading it breaks `If-None-Match`. `X-Datamk-Execution` rides along:
  it mirrors what the pre-auth health route already exposes, leaks nothing
  new, and tells an agent the rows moved under it. Headers only — the row
  body stays a bare JSON array; wrapping it breaks every existing consumer.
- **`datamk context`** — new command emitting the same serialized struct to
  stdout (`--out` to write). No server, no port, no token; commit it, host it
  statically, paste it into an agent's context. The portable artifact
  additionally carries the execution number + snapshot pin, a digest of the
  `cell.yaml` it was emitted from, and `emitted_at` — and never carries poll
  telemetry, which is a lie the instant the file is written. **Pinless ⇒
  draft, by definition.**
- **`datamk serve --no-data`** — the data route is simply not mounted.
  Unmounted routes return **404, not 403** (403 promises a door that
  exists). The 404 body and the document's `data` block carry the same
  engine-emitted sentence: rows are not served by this endpoint by design;
  fetch via `data.channels` (`channels: []` stays empty when the profile
  declares none — never fabricated). The document omits the probe's
  `values` lists (§5): value lists are row-derived data, and shipping them
  from a mode whose whole point is that rows stay put would exfiltrate a
  projection of the withheld rows. `coverage` (min/max dates, row count)
  stays — the aggregate that turns an empty mesh answer into a diagnosable
  miss, and it names no entity. The `query` block does **not** depend on
  `--no-data` (amended; see "Amendment" below) — `data.served_here: false`
  and the 404 already carry "not here."
- **Auth: the same tier as the data, exactly `authorize()`.** The document is
  the map — grain, columns, prose, upstream refs. No lower "docs" tier, no
  pre-auth serving, no crawling. `access.shareable` keeps its single meaning
  (default-deny gate) and is not overloaded to mean "docs yes, rows no." The
  pre-auth `/` health route stays exactly as it is — one liveness fact,
  extended by nothing.
- The document is built from the same visibility-filtered `routes` map the
  router dispatches on (`src/serve/mod.rs:122-126`). Today
  `openapi::generate` independently re-applies the same predicate
  (`src/serve/openapi.rs:9-11`) rather than reading that map — the
  `/context` builder must not become a third call site; all three read the
  one map. A `private` export appears nowhere, in any form, not even as a
  name.

### 5. Provenance and probes

- The poller fetches `RunSummary` on **every** poll tick, decoupled from the
  swap check — gating it behind the swap branch orphans any summary that
  lands after `LATEST` advances (the summary is written after
  `publish_execution` returns, `src/engine/mod.rs:704-730`). Handlers touch
  no store and no DuckDB: `Store::get` (`src/store.rs:281`) rides the
  joined `block_on` bridge (`src/store.rs:266`) and parks a worker on a
  network round-trip.
- **Admitted to the wire:** `execution`, `snapshot_id`, `verify_outcome`,
  `started_at`/`finished_at`, `datamk_version`, `data_as_of` (from
  `ducklake_snapshots('lake')`, cached at swap).
- **Never on the wire:** `export.source` (the private/public seam — the
  contract comment on the field, `src/config/schema.rs:44-47`), connection
  names, `staged_rows`,
  `bytes_scanned`, transform filenames/durations, upstream `table` names,
  resolved storage/catalog URIs, roles/principals, private exports.
- **Swap-time probe** (never on the request path; omit on timeout, never
  block serving): `coverage` (min/max of date-typed grain columns, row
  count) and `values` for low-cardinality string grain columns (`LIMIT 51`;
  ≤50 back ⇒ `values` listed with `values_complete: true`; 51 back ⇒ `values`
  omitted, `values_complete: false`). These turn the worst agent
  failure — an empty result read as a legitimate zero — into a diagnosable
  miss.
- **The probe also captures one real row's grain values** — a single
  `SELECT {grain columns} FROM {export} LIMIT 1` at swap time — and from
  them emits `observed.probe.example_request`, the grain-filtered sibling
  of §2's `sample_request` (e.g. `flight_spend@1?month=2026-06&limit=10`).
  Jointly drawn from one row, never composed from the per-column `values`
  independently — independently-picked values can name a combination that
  co-occurs nowhere, manufacturing the exact empty-result-as-zero failure
  the probe exists to kill. It lives in `observed` because it is a
  measurement; it is emitted only when every grain column got a value and
  omitted otherwise — never a placeholder, which an agent pastes literally.
  Both request fields ship only after §7's `ORDER BY` fix: an example with
  `limit=10` over nondeterministic order teaches an agent a sample is
  stable when it isn't.
- **What `verify` honestly backs, on today's code:** column existence and
  grain uniqueness are checked against real rows (`src/verify.rs:93-141` —
  the grain check scans the full table); declared types become real checks
  only when §7's warn→error promotion lands. The one row-derived fact the
  existing checks already compute is the scanned row count
  (`grain_counts`, `src/verify.rs:353-360`) — admitted to the wire as
  `observed.rows`. No `sample_checked` boolean: `verify` inspects no row
  *values* today, and a flag with no backing check is precisely the
  claim-without-a-measurement §2 forbids. `verify_outcome` is `"passed"` by
  construction when present (`run` publishes only after verify succeeds,
  `src/engine/run_summary.rs:24-28`) — its honest wire meaning is
  "provenance present ⇒ a verified build stands behind this document,"
  never a tri-state.
- **Upstream edges: nominal, one hop.** `{ref, version}` from
  `Source::Cell` — no `table` (the upstream owner's to disclose, on the
  upstream's own document, under the upstream's own auth). A **proposed**
  optional `url:` on the profile's `CellLocation`
  (`src/config/schema.rs:775-779` — today exactly `catalog` and `storage`;
  this field does not exist yet) makes an edge walkable; it is environment,
  optional, and leaks no storage.

### 6. The universe of cells

The remaining question: an agent holds one cell's URL — how does it learn
which cells exist at all?

**Refused: a hosted registry service.** A server-side index of cells is a
control plane; no-control-plane is the thesis. A transitive server-side
lineage walk is the same thing wearing lineage's clothes (it requires the
serving plane to hold credentials to other serving planes). And `serve`
never serves the manifest — a data-plane route that enumerates other cells
is a registry endpoint and pre-auth crawl bait.

**Principle: the universe is a document — a hint, never an authority.**

- **The mesh manifest**: a static JSON document with a closed shape
  (`deny_unknown_fields`), hosted anywhere (bucket, repo, intranet page):

  ```json
  { "datamk_mesh": 1, "generated_at": "…",
    "cells": [ { "name": "…", "url": "…", "description": "…",
                 "exports": [ { "name": "…", "version": "…", "contract": "…", "bound": false } ],
                 "context_digest": "…", "auth_hint": "…" } ] }
  ```

  Everything beyond `{name, url}` is **copied from each cell's own context
  document by the emitter** — never typed into the manifest by hand. One
  owner per string: the manifest is a digest-stamped cache and the cell's
  document always wins. The copied summary is what lets an agent *route*
  ("which cell answers revenue questions?") without N cold fetches — without
  opening a second authoring surface the §3 ratchet cannot reach.
- **What derivation can and cannot do, honestly.**
  - The **deploy target** is the only complete derivation: the Kubernetes
    deploy (ADR 0002) labels what it deploys and enumerates names *and*
    serving URLs by selector.
  - The **store path** (`datamk mesh emit --store <prefix>`) is a **name
    census, not a manifest**. It works only where cells share a parent
    prefix by convention; only on S3/GCS (`Store` matches no local scheme —
    `src/store.rs:232`); it requires a new one-level delimited listing
    method on `Store` (today's `list_names`, `src/store.rs:349-370`, lists
    recursively and collapses results to final path segments — a pile of
    indistinguishable `LATEST` strings); and it **cannot produce `url`** —
    the store knows where data lives, never where `serve` is reachable.
    `url` is operator-supplied via template
    (`--url-template "https://{name}.data.internal"`): authored config,
    deduplicated across N cells rather than eliminated. The `Store` listing
    method is named, scoped work (§7), not assumed capability.
  - Hand-authoring the `{name, url}` list remains the fallback for
    heterogeneous estates, with the anti-rot cost accepted openly.
- **The manifest is never a token-routing authority.** The fan-out threat: a
  manifest that remaps a known cell name to an attacker's URL harvests every
  bearer token in the mesh in one pass. Token→host bindings live in the
  agent's own configuration; the manifest supplies candidates. Client rules:
  https-only, never reuse a credential across hosts, treat a changed `url`
  for a known name as a hard stop, and stamp all copied prose with its
  origin cell. (The profile's proposed `url:` on `CellLocation` (§5) is
  different in kind: profile-authored, therefore trusted — the same trust
  boundary as every other environment binding.)
- **Tokens are the real day-one limitation, named here rather than in a
  footnote.** Thirty cells means thirty per-cell principals files and thirty
  bearer tokens provisioned before an agent's first useful minute. v1 offers
  `auth_hint` per manifest entry — an opaque credential *name* the agent
  resolves in its own secret store, never a token — and otherwise declares
  token provisioning out of scope. If the live use case stalls here, the
  evidence moves shared-principals tooling or the aggregator up the roadmap.
- **The aggregator, later.** A client-side process (`datamk mesh serve`)
  reading the same manifest, fanning out over plain HTTP with the operator's
  tokens, serving the union — is the natural home for a single MCP server
  and for `/.well-known/*` conventions, because it is the first process that
  owns a "site." It is not v1, and it never lives server-side.

The honest caveat ships in the document: a walked mesh is N independently
authored contracts. Nothing reconciles `revenue` in cell A against cell B
(`ARCHITECTURE.md:175-178`); the manifest makes the union *inspectable*,
which is the precondition for reconciliation as a deliberate later feature —
not the thing itself.

### 7. Prerequisites — latent bugs this feature converts to scheduled

Agents are the first consumer that triggers these systematically; they land
before or with the endpoint:

- **`ORDER BY` on the paginated read** (`src/serve/mod.rs:383-386` has none):
  limit/offset over nondeterministic order silently skips and double-counts
  rows for any paginating aggregator. Sort key: the declared grain.
- Cap `offset` (parsed, never capped — `src/serve/mod.rs:378-381`); consider
  keyset pagination over the grain.
- **Unknown query params are rejected (400), not silently dropped.** Today a
  non-grain param is ignored (`src/serve/mod.rs:361-371`, pinned as intended
  by `non_grain_params_are_ignored_in_where`, `:466-473`): an agent passing
  `?revenue=999` gets unfiltered rows it will confidently read as a
  filtered subset — a false-confidence failure, not a fetch failure. A
  behavior change, named as such.
- A fixture test binding the document's `query` claims (`filter_semantics`,
  limit default/max) to the constants and template in `build_query`
  (`src/serve/mod.rs:18-19`, `:361-386`) — hand-restated prose beside
  enforcing code orphans silently on the next change to either.
- `/openapi.json` honesty: version → document digest; grain params typed from
  the declared schema, not `string` (`openapi.rs:36`); unknown types emit
  `{}` + `x-datamk-declared-type`, never a fabricated `string`
  (`openapi.rs:69`); document the real 401/403/404/500 responses.
- `verify` type mismatch is **promoted from warning (`src/verify.rs:103-108`)
  to hard error**. This is a compatibility break for any cell currently
  relying on the lenient warning — it ships as its own named change with a
  release note, not folded into cleanup. Fallback only if the break proves
  too sharp in practice: warnings ride `RunSummary` into the document's
  `notes[]`. The default is the error; a declared type asserted to a machine
  must be true.
- `serve` gains minimal rate limiting (or documented reverse-proxy
  guidance) — agents fan out and retry tirelessly, and today nothing
  throttles them.
- `Store` grows a one-level (delimited) listing method for mesh emission
  (§6) — new, scoped API surface on `src/store.rs`; S3/GCS only.
- An in-process HTTP smoke test of the serve surface (none exists today;
  the only live curl is in the docker-only `kind_e2e` harness) — for the
  existing routes as well as the new document.

### 8. Security posture

- **Prompt injection is the new surface**: cell prose is the first place
  untrusted author text lands in a trusted agent context — cross-tenant, in
  a mesh (a cell you don't own, read by an agent you do). Prose is
  length-capped, served as data; any future aggregator must stamp every
  string with its origin cell and never flatten N cells' prose into one
  undifferentiated blob.
- A serialization guard test in the `run_summary.rs:144-159` mold: a
  fully-populated document from a cell with S3/GCS/connection/cell sources
  must contain no `s3://`, `gs://`, `postgres:`, `credential`, `secret`,
  `password`, `key_id`, `account`. `Resolved*` types never derive
  `Serialize`; that absence is the compile-time barrier.
- **datamk never accepts a query from a caller.** No filter expressions, no
  projections, no `order_by` params, no natural-language questions on this
  socket, in any version. An agent generating SQL runs it in its own DuckDB
  against the storage plane — its process, its credentials, its bill.
  `sample_request`/`example_request` do not soften this: they emit only
  calls the closed grammar already accepts — the server gains no new
  parser, the caller no new expressive power.

## Alternatives considered

- **"Semantic layer" naming** — contradicts the published stance in three
  places (`docs/concepts/composable-data-products.md:194`, the live blog,
  `VISION.md:151-153`) and promises cross-cell reconciliation the payload
  does not perform. Refused.
- **Metrics DSL in `cell.yaml`** — unexecuted, unverifiable SQL claims; the
  dbt-metrics graveyard. A metric is an export.
- **MCP server per cell** — 40 cells would hand an agent 40 servers, which is
  the problem restated. MCP belongs to the future client-side aggregator;
  the per-cell surface stays the dumbest fetchable document that works.
- **Standalone definitions product** — a catalog without verification; the
  moat is that a build stands behind the document. For pure
  design-first dictionaries with no pipeline, a YAML file in git serves that
  customer at zero cost; recommended without embarrassment.
- **Enriching `/openapi.json` as the primary surface** — generic tooling
  strips `x-` fields and the meaning ends up crammed into description
  strings; OpenAPI stays the callable HTTP spec, secondary, derived from the
  same structs.

## Consequences and risks

- Prose content is unverifiable by construction; the ratchet bounds drift
  (orphan-kill, digest, promotion lint) but cannot eliminate it. Accepted
  openly.
- Draft documents (pre-first-build) lean entirely on the four prose fields —
  every derived win (coverage, values, `data_as_of`, `grain_verified`) is
  absent — which is why draft never ships hosted (§1) and the promotion
  lint is mandatory, not advisory.
- The document is a contract the moment an agent scripts against it; field
  names are stable on ship, guarded by the golden test.
- Rough sequencing, five PRs: (1) prerequisite fixes + serve smoke tests —
  with the `verify` type-mismatch promotion and the unknown-param 400 each
  called out inside it as breaking changes; (2) derived document + `datamk
  context` + `sample_request` + the data-route back-link headers (the
  request fields gated on (1)'s `ORDER BY`); (3) meaning
  fields + ratchet — the one-way door on the `schema:` value shape (the
  change ripples into `src/verify.rs:97` and `src/serve/openapi.rs:40`,
  which read bare type strings today), and the ratchet is four distinct
  checks (orphan-kill, release-time digest warning, supported⇒description,
  supported⇒verified-build), each its own logic and tests; (4) probes —
  including the joint-row `example_request` — + `--no-data`; (5) mesh
  emission — the new `Store` listing method plus the URL-template decision.

## Premises

This decision holds while: (a) the live use case is per-cell orientation over
many complex products — falsified if agents need dynamic cross-product
aggregation no finite export list covers, in which case the answer is the
storage plane with vended read-only credentials, not a metric DSL; (b) "gate
zero" fails — an agent given only today's `/openapi.json` for `flight-spend`
cannot answer the five semantic questions (which column is invoiced, `budget`
vs `flight_budget`, `flight_id = -1`, `month` timezone, last built); if it
passes, this is a docs page — run before writing code; (c) the ratchet ships
with the prose fields — without it the anti-rot objection becomes correct
within quarters; (d) day-one estates are enumerable by manifest — falsified
if the observed agent failure is choosing *which* cell among dozens, which
moves the aggregator up the roadmap and makes per-cell token provisioning
the real blocker; (e) the `flight-spend` upstream sqlmesh model does not
already carry column descriptions in machine-readable form — if it does, the
right v1 authoring feature is an import path, not hand-authoring; (f) the
"a hosted `/context` is never draft" rule falls out of `serve` requiring a
published execution — if a serve-before-first-build mode ever ships, that
rule needs its own enforcement rather than holding by construction.

**Checked 2026-08-05.** (b) failed as required: a fresh agent given only the
derived `/openapi.json` for `flight-spend` answered none of the five
questions from the document — it guessed `invoice_amount` by name but
called the formula unknowable, misread `budget` as period-scoped rather
than prior-spend-adjusted, could not decide whether `flight_id = -1` is
billable, could not name `month`'s calendar (truth: advertiser-local, not
UTC), and refused to sign off the CFO number on exactly those grounds. Its
recovery plan also paginated limit/offset with no `ORDER BY` — live
confirmation of §7's first prerequisite — and reinvented provenance from
`max(batch_time)`, which is the `observed` block's job. (e) holds: the
upstream sqlmesh model (`mountain/sqlmesh`,
`models/dw-main-silver/invoice/flight_spend.sql`) carries a model-level
description and no per-column machine-readable descriptions (no
`column_descriptions`, no `register_comments`); the meaning lives in CTE
comments no importer reaches. (f) holds by construction: `serve` on the
unbuilt cell refuses to start (cannot attach a nonexistent catalog
read-only) and binds no listener. (a), (c), (d) are commitments about the
live use case and the build, not locally testable — they stay open as the
reversal conditions above.

## Amendment (2026-08-10): the `query` block is unconditional

§2 and §4 originally said the per-export `query` block is *"omitted under
`--no-data` — it describes HTTP affordances that do not exist there,"* and
§2 separately says the digest covers `declared`, `query` included,
deliberately excluding `observed` so a data refresh never churns it. Held
together those two statements contradict themselves the moment `query`
tracks a *serving* fact (`--no-data`) rather than an *interface* fact:
`interface_digest` moves when `--no-data` flips, even though nothing about
the declared interface changed — the exact class of churn §2's digest
scoping exists to prevent. Found during the same work that closed the
`served_here`/`direct_attach` divergences between the two doors
(`datamk context` vs. hosted `/context`); flagged by a design partner
building against both.

This reverses course: `query` is dropped from being conditional on
`--no-data` and becomes unconditional interface grammar, present for every
export that is not bound (`Export::bind`, see the binding-model amendment
below — at the time this section was first written that export shape was
spelled `materialize: never`; the mechanism this paragraph describes did not
change, only the `cell.yaml` syntax naming it did). Two mechanisms already
carry "not here": `data.served_here` (§2, now itself corrected to be honest
under a second reason a route can be absent — every export bound directly to
an existing object — not just the flag) and the unmounted-route 404 (§4,
same engine-emitted sentence in both places). `served_here` is deliberately
still part of `interface_digest` — a servability change is a genuine fact
about this surface, the same reasoning that already put `channels` in
`data` — so the digest is **not** claimed to be fully stable across
`--no-data`; it still moves for that one, honest reason. What's fixed is
narrower: `query` was a *second*, redundant mechanism saying the same "not
here" a second way, and *that* movement — a data-format grammar field
churning on a deploy flag with no interface change behind it — was the
actual contradiction with §2's digest-scoping rule. It's gone; `served_here`
alone now carries the "mounted here" fact into the digest.

`query` still nulls for a bound export, unchanged — that omission is a
genuine interface fact (this export has no query affordance, ever,
regardless of any flag, since datamk owns its contract, not its rows), not a
serving-mode artifact, and stays governed by whether the export is bound,
never by `--no-data`.

**Consequence, accepted openly:** a `--no-data` server's `interface_digest`,
`ETag`, and `/openapi.json` `info.version` move on upgrade to this behavior
(same bytes served, first request after the binary changes) — a one-time
shift, not a recurring one. `--no-data` toggled at deploy time still moves
the digest afterward too, same as before this amendment — only now via
`served_here` alone, not `served_here` and `query` disagreeing about
whether that was one fact or two.

## Amendment (2026-08-10): `materialize: never` is retired; virtual cells become bindings

Everywhere above (§2's `query` description, the previous amendment) an
export with no materializing transform is described as `materialize:
never`. That keyword no longer parses to a working cell — `datamk run`
refuses it with a migration error naming the offending export and
transform. The mechanism this document describes throughout —
`query: null`, `data.served_here` false for that route, the unmounted-route
404 with the engine-emitted "not served by this endpoint by design"
sentence, `status: verified_at_source` when `datamk verify` live-checks it
— is **unchanged in shape**. Only the `cell.yaml` syntax that produces it,
and what a datamk process is willing to run for it, changed.

**Why.** SQL that isn't materialized is fine only if something runs it: the
Builder computes and stores it (an ordinary materializing transform), the
warehouse hosts it as a view (a `Connection` source with `table:` pointed at
one), or a datamk process executes it per request (not offered — datamk
doesn't run ad hoc query jobs against a caller's request). `materialize:
never` was none of those — it declared a transform, parsed a `SELECT`, and
then guaranteed nothing would ever run it again after the one dry-run
`datamk verify` performed at check time. Every semantic the document above
attaches to that export (`status`, a live-checked interface) was a promise
the mechanism itself didn't keep between checks.

**The replacement.** `Export.bind: <source name>` names an existing
`sources:` entry directly — no transform, no `SELECT`, nothing datamk
computes or stores. Mirroring how `sources:` already splits contract from
environment: *which* upstream object (the source's logical name) is
contract, in `cell.yaml`; *where* it resolves (project/dataset/instance) is
environment, in the profile's `connections:` map, exactly as it already was
for a normal `sources:` entry. Bindable today: a raw file (`Source::Raw`) or
a `Connection` source with `table:` set — not a `query:`-shaped connection
(ad hoc SQL nobody runs, the same problem `materialize: never` had) and not
another cell's table (`Source::Cell`; read it through a materializing
transform instead). A cell whose every export is bound has no
materializing transforms and therefore no snapshot to commit — `datamk run`
refuses it outright (§2's "unbuilt cells assert their status positively"
now has a permanent resident: a cell that will never be anything but
`draft` or `verified_at_source`, never `verified`), and its document carries
`NOTE_VIRTUAL_CELL` pointing at `datamk verify` as the one command that
moves it off `draft`.

**Downstream of this decision, landed in the same arc:**

- **verify's type authority** (issue #9): a bound export's declared type is
  checked against the connector's own native metadata (BigQuery's
  `INFORMATION_SCHEMA.COLUMNS.data_type`) when one exists, not DuckDB's
  `DESCRIBE` of the bound session view — DuckDB's rendering can lose
  information the warehouse's own type didn't (a wide BigQuery
  `NUMERIC`/`BIGNUMERIC` degrades to DuckDB `VARCHAR`; checked against the
  warehouse authority, it correctly passes a declared `decimal`). Postgres
  and Snowflake run no metadata job (existing connector architecture, ADR
  0010) and keep DuckDB's `DESCRIBE` as the only authority — not a lesser
  fallback, there is genuinely no other source of truth to consult there.
- **`observed.source_descriptions`** (issue #10): upstream column
  descriptions, source name -> column name -> description, from the same
  metadata job §9 already pays for. Machine-observed, timestamped,
  digest-and-profile-gated exactly like `observed.source_check` (same
  `.cell/`-persisted, artifact-shipped, fail-closed pattern) — never folded
  into `declared` (author-reviewed prose) and never into `docs:`
  (file-backed, allowlist-validated, feeds `description_digest` at release):
  an upstream comment edit must never move datamk's own release gate.
- **A bound cell becomes deployable** (issue #11): the deploy pre-flight no
  longer refuses an all-bound cell outright — only when the target cannot
  host the long-lived Server at all. On Kubernetes, the one-shot init Job
  that otherwise initializes the catalog ahead of the Server is not
  rendered or applied at all for an all-bound cell (`render_init_job`
  returns `None`, the same `Option` idiom `cronjob` already uses) — a
  rendered Job would only ever run `datamk run`'s own refusal inside the
  pod, fail after its retries are exhausted, and leave a partial apply
  behind (ConfigMap applied, Service and Deployment never reached). A
  target that always reports both workloads (Kubernetes) separately
  refuses `schedule:` set together with an all-bound cell (nothing for a
  Builder to build; `datamk run` already refuses it, so the CronJob would
  crash-loop every scheduled tick) and never reports a Builder workload for
  such a cell. The open-endpoint refusal (§8) names the document itself as
  the payload for a bound cell — declared columns, grain, and prose (which
  can itself name upstream fields) is exactly what an anonymous caller
  gets, even though no rows ever are.
- **`declared.exports[].route`'s documentation, corrected** (issue #12): it
  was documented as unconditionally "the serving route key." For a bound
  export it never was — `/openapi.json`'s paths already excluded it, making
  `/context` the surface that disagreed. `route` stays present for every
  export (it is also the export's docs `target`, ADR 0013 §5, independent
  of servability), but its meaning is now stated precisely: `query` (`null`
  for exactly the bound exports, by construction) is the field that answers
  "does `GET /{route}` exist," never `route`'s mere presence.

Internal identifiers renamed for the same reason (mechanical, no behavior
change): `never_backed_routes` -> `bound_routes`, `note_never_backed` ->
`note_bound_export`. `check_no_materialize_never` and
`describe_never_offender` keep their names — they name the literal rejected
`materialize: never` keyword the migration error matches on, which is
correct as long as that error exists.

## Amendment (2026-08-11): `datamk_context: 2` — bindings, measurements, and a rename

From a design partner's report of consuming a live bound cell. Four changes,
one of them breaking.

- **`declared.exports[].binding`** (additive). A bound export's target was
  machine-invisible: `query: null` said "not here" and nothing said where.
  The object name existed only in `docs:` prose, and `data.channels` — an
  operator hint from the profile, free-form by design — gave the dataset at
  best. Writing a query against a bound cell meant reading English, which is
  the one thing this document exists to stop. Now: `{source, object,
  connection}`, present iff `query` is null, so the two are complements.
  **Values are verbatim `cell.yaml`, never profile-resolved** — `table` is
  env-expandable and `Declared` is hashed wholesale into `interface_digest`,
  so a resolved value would fold the environment into the digest and churn it
  per deployment; a templated table ships as `${DATASET}.fct_x`. A `cell:`
  source is rejected at resolve time (`verify::validate_bound_exports`) and an
  unresolvable name discloses no object, so neither can leak an upstream's
  table through this field — the §5 disclosure boundary is unchanged.
  `materialize: "never"` was requested as a third key and declined: that
  strategy was removed by the amendment above, and presence of `binding` is
  already the positive assertion.
- **`observed.source_check.exports`** (additive). `grain_verified: true` and
  `outcome: "passed"` asserted a result while `verify` computed the counts
  behind them, compared them, and discarded them (`verify::grain_counts`).
  Route key -> `{check, grain, rows, distinct_grain}`, persisted through
  `.cell/source_check.json` under the same fail-closed digest+profile gate as
  the record itself, and visibility-filtered on the way to the wire.
  Timestamped by the enclosing `checked_at`: one pass, one time. A grainless
  export contributes no entry — no check ran on it, and §2 forbids inventing
  one. Deliberately not hung off `observed.exports` (`ExportProbe`), which is
  lake-row-derived at swap time and would be re-meaned.
- **`MeshExport.bound`** (additive, mesh manifest). The manifest carried
  `{name, version, contract}`, so an agent routing off it picked a bound
  export and hit the 404 the document could have warned it about. Copied from
  `binding`'s presence, like every other manifest field. The emitter now
  accepts `datamk_context` 1 or 2 — the v2 rename touches nothing it copies.
- **`declared.docs[].path` -> `source_path`** (**breaking**; the reason
  `datamk_context` is 2). A cell.yaml-relative filesystem path named `path`,
  in a JSON document served over HTTP, reads as a relative URL and 404s for
  anyone who tries it — the same false affordance already corrected for
  `route` in the amendment above, in the same document, on the same reasoning.
  Serving the pages at their own route was rejected again: §4 is one document,
  one route, and `?include=docs` already delivers content.

Alongside, on the OpenAPI surface (no document change): `/context`'s `200`
carried a bare description string and no schema at all, so nothing in the
spec said `include=`'s content lands at a **top-level key named for the
section** rather than inside `included` — the reported failure was an
iterator written over `included`, which holds section names. The response now
has a real schema, pinned to `ContextDocument`'s top-level keys by a test,
and the `include` parameter states the landing rule. Data path items also
carry `x-datamk-version` and `x-datamk-contract`, which previously existed
only as prose inside `summary`.

**Declined: a cell-level semver alongside `info.version`'s digest.** There is
no cell version in `CellDef`, and adding one creates a second identity axis
that will drift from the per-export semvers within a single release, with no
policy for who bumps it. Exports version independently — that is the point of
the interface. `info.version` = interface digest remains correct (OpenAPI
attaches no semantics to that field); the real gap was that `contract` was
unreachable from `openapi.json`, which the extensions above close. Reverses
if consumers need to pin the cell as a whole — and the answer then is a
manifest of export versions, still not a new semver.

**Declined: a structured `caveats` array per export, and docs-by-default.**
The motivating hazard is real — `mrr` and `total_infra_cost_usd` both
`decimal`/`USD`, with "not period-aligned" living only in prose behind an
optional flag, so subtraction looks fine to a JSON-only agent. But a
free-text `rule` with an engine-unenforced `severity` is prose in a JSON
wrapper: it drifts exactly like `observed.source_descriptions` already does,
and nothing can check it. Docs-by-default was rejected separately — it breaks
the ETag variant split (ADR 0013 §6), unbounds the default response, and
prose is precisely what the agent in question won't parse. The hazard stays
open. The only fix that isn't docs-in-JSON is a typed column property the
engine can verify — a period/point-in-time attribute beside `unit`, making
"these two are not comparable" derivable rather than asserted. Not decided
here.

## Amendment (2026-08-27): `GET /context/{route}`

§4 said "one document, one route" and refused per-export routes. The first
production consumer — an MCP tool answering one question — showed the cost:
a 48 KB fetch and client-side filtering to get one export's contract and
page, growing with every export. The document stays one document; it gains
one more door onto a slice of itself:

- `GET /context/{route}` returns the **same schema**, with `exports[]`
  reduced to the named export and `docs[]` to the cell page plus that
  export's page. `?include=docs` works as on `/context`. Own ETag variant
  (`~export.<route>`). 404, post-auth, names the routes that exist. Bound
  exports are addressable (they have no data route, but they have a
  contract).
- `/openapi.json` advertises it with `route` enumerated from the
  discoverable exports.
- `datamk context --export <route>` is the portable twin.

Not added: `/docs/{route}` as a separate text/markdown door — the page
rides `docs[].content` under `?include=docs` on the narrowed document,
which is one shape for consumers rather than two. Reopen if a consumer
needs the raw page without JSON.

