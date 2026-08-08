# The context document: make your cell legible to agents

Point an AI agent at a bare API and it gets column names and types — then it
guesses at everything that matters: which column is the real revenue number,
what `-1` means, whether a date is UTC. Each wrong guess produces a
confidently wrong number, silently.

Every Datamake cell serves a **context document**: the cell's interface made
machine-readable — what `/openapi.json` is to an API, this is to a data
product. It carries the schema *with meaning*, the exact query grammar the
endpoint accepts, measurements probed from the real rows, and the build
provenance that says whether the numbers can be trusted. An agent orients
itself in one request — one that can optionally carry the cell's long-form
docs pages too (`?include=docs`, below) — where the data can't be trusted
yet, the document says so instead of letting the agent guess.

There is nothing separate to adopt or maintain. The document is a projection
of the cell: write the cell and the context exists; build the cell and it
becomes trustworthy.

## Fetch it

Two doors, same JSON:

```bash
# Served, next to the data (same auth as the data routes):
curl -H "Authorization: Bearer $TOKEN" https://orders.data.internal/context

# Portable — no server, no port, no token. Commit it, host it statically,
# or paste it straight into an agent's context:
datamk context -f cell.yaml            # stdout
datamk context -f cell.yaml --out context.json
```

Every data-route response also points back at the map — 200 and 404 alike,
because a wrong guess is exactly when the map matters:

```
Link: </context>; rel="describedby"
X-Datamk-Context-Digest: 5eae8045…    # interface digest; changes when the
                                      # interface changes, not when data refreshes
X-Datamk-Execution: 47                # published mode: the rows moved under you
```

The digest is also `/context`'s `ETag` (send `If-None-Match` to get a 304)
and `/openapi.json`'s `info.version`. Requesting `?include=docs` (below)
gets its own `ETag` variant, so caching stays correct per variant; the
digest itself never moves for a prose-only change.

## What's inside

The document has two regions that are never mixed, and that separation is the
whole point:

- **`declared`** — author claims: exports, grain, schema with descriptions
  and units, the query grammar. What the author *says* the data means.
- **`observed`** — machine facts: build provenance (execution, snapshot,
  verify outcome, when the data actually last moved), the execution/freshness
  actually attached for each upstream `cell` source, and measurements probed
  from the real rows. What the machine *measured*.

An agent can never mistake a claim for a measurement. Absent facts are
omitted or `null` — never fabricated, never zeros.

```json
{
  "datamk_context": 1,
  "cell": "orders",
  "status": "verified",
  "grain_verified": true,
  "declared": {
    "description": "Daily order revenue by region.",
    "exports": [{
      "name": "orders_daily",
      "version": "2.1.0",
      "route": "orders_daily@2",
      "contract": "supported",
      "description": "One row per (order_date, region) with the summed order revenue.",
      "grain": ["order_date", "region"],
      "schema": {
        "order_date": { "type": "date" },
        "region":     { "type": "string" },
        "revenue":    { "type": "decimal", "unit": "USD",
                        "description": "Gross order revenue, before refunds." }
      },
      "query": {
        "filters": ["order_date", "region"],
        "filter_semantics": "exact equality only — no ranges, no operators, no non-grain columns",
        "limit_default": 100, "limit_max": 1000, "offset_max": 1000000,
        "sample_request": "/orders_daily@2?limit=10"
      }
    }],
    "upstreams": [{ "ref": "flights", "version": null }]
  },
  "observed": {
    "provenance": {
      "execution": 47, "snapshot_id": 12, "verify_outcome": "passed",
      "finished_at": "2026-08-06T10:00:05Z", "data_as_of": "2026-08-06 10:00:04+00"
    },
    "upstreams": [
      { "ref": "flights", "execution": 41, "data_as_of": "2026-08-04 06:00:11+00" }
    ],
    "exports": {
      "orders_daily@2": {
        "rows": 4,
        "coverage": { "order_date": { "min": "2026-06-01", "max": "2026-06-02" } },
        "values":   { "region": { "values": ["eu-west", "us-east", "us-west"],
                                  "complete": true } },
        "example_request": "/orders_daily@2?order_date=2026-06-01&region=us-east&limit=10"
      }
    }
  },
  "data": { "served_here": true, "channels": [] },
  "notes": []
}
```

A few parts earn special attention:

- **`query`** states the served affordances *exactly* — the same constants
  the server enforces, so the claims cannot drift from the behavior. This is
  what stops an agent inventing `?order_date__gte=`. Anything outside the
  grammar is rejected with a 400 that names what's allowed — never silently
  ignored (an ignored filter returns unfiltered rows an agent will
  confidently read as a filtered subset).
- **`sample_request`** is the smallest legal call; **`example_request`** is
  its grain-filtered sibling, drawn jointly from one *real* row at build
  time — pasting it returns data, never an empty page. The `coverage` and
  `values` measurements turn the worst agent failure — an empty result read
  as a legitimate zero — into a diagnosable miss ("June is outside the data's
  range" instead of "revenue was zero").
- **`status`** is `verified` only when real provenance — a published,
  verify-gated build — stands behind the document. A cell that has never
  been published serves `status: "draft"` with an engine note saying exactly
  that. Draft never wears the verified costume.
- **`declared.upstreams` vs. `observed.upstreams`** close a real gap for
  composed cells: `declared` carries only the author's pin (`version`,
  usually `null` — most cells float on whatever `catalog/LATEST` points at
  when they build), while `observed` carries what the Builder actually
  attached — the resolved `execution` and the upstream's `data_as_of` at
  that moment. A cell can be `status: "verified"`, built minutes ago, while
  reading an upstream artifact that's days stale because *that* cell's build
  has been failing; `observed.upstreams` is what makes that visible instead
  of silent. `execution` is `null` for a direct-attach upstream (no
  publish-store execution number exists to report) — never fabricated.
  `datamk status` narrates the same measurement under `upstreams (at
  LATEST):`.

## Author the meaning

Meaning is the one thing the machine can't derive, so it's the only new
authoring surface — exactly four fields, rationed to where wrongness produces
a confidently wrong number rather than an error:

```yaml
cell: orders
description: Daily order revenue by region.        # one line

interface:
  - name: orders_daily
    version: 2.1.0
    description: One row per (order_date, region) with the summed order revenue.
    grain: [order_date, region]
    schema:
      order_date: date            # obvious columns stay bare type strings
      region: string
      revenue:                    # non-obvious ones take {type, unit, description}
        type: decimal
        unit: USD                 # structured token, never prose — the #1
                                  # silent-wrong-number source
        description: Gross order revenue, before refunds.
```

Every existing cell parses unchanged — the bare `column: type` shape still
works everywhere. Guidelines:

- **Describe the non-obvious.** What one row means, which column is the
  billable one, what a sentinel value like `-1` marks, whether a date is
  UTC or local. Skip `advertiser_id` — an agent can read.
- **`unit` is a token** (`USD`, `ms`, `rows`), max 16 characters, no
  whitespace. Prose belongs in `description`.
- Prose is length-capped at parse time (one line for the cell, ~2 sentences
  for exports and columns). These are orientation, not documentation pages —
  and the caps also bound the prompt-injection surface, since cell prose is
  text that lands in an agent's context.

### The anti-rot ratchet

Prose beside code rots. These fields ship with enforcement that bounds the
drift:

1. Prose lives on the export in `cell.yaml` and nowhere else — no side
   files, no annotation store. It versions with the schema it describes.
2. Describing a column the source no longer has is a **hard `verify`
   failure** — a rename or drop kills the orphaned sentence instead of
   letting it describe a ghost.
3. `datamk release` records a digest of each route's meaning prose. Changing
   a description without bumping the version draws a warning at the next
   release — a change in meaning is a MAJOR change.
4. An export promoted to `contract: supported` **must** carry a non-empty
   description — `verify` fails otherwise. Friction lands exactly on the
   deliberate promotion gesture; experimental exports need nothing. A
   `docs:` page (below) does **not** satisfy this — an agent reads
   `description` before it ever fetches a page.

## Long-form docs pages

The four meaning fields above are capped at a sentence or two on purpose —
they're orientation, not documentation. When there's genuinely more to say
(*why* a column behaves the way it does, not just what it's called), point
at one long-form page per level, cell and export, additive to `description`:

```yaml
cell: orders
description: Daily order revenue by region.
docs: docs/overview.md                              # cell-level, optional

interface:
  - name: orders_daily
    version: 2.1.0
    description: One row per (order_date, region) with the summed order revenue.
    docs: docs/orders_daily.md                       # export-level, optional
```

One relative path per level — no lists, no globs, no per-column docs, no
remote URLs. Each page is capped at 64 KiB, 256 KiB total across the cell
(the Kubernetes ConfigMap that ships cell content to a deployed Server is
capped at 1 MiB, shared with `cell.yaml` and every transform's SQL) — an
oversized, unreadable, empty, or non-UTF-8 page is a hard error at parse
time, never a silent truncation.

**There is no `/docs/:name` route.** Docs are delivered inline in the
context document, on request:

```bash
curl -H "Authorization: Bearer $TOKEN" \
  "https://orders.data.internal/context?include=docs"
```

```json
{
  "...": "... every field the default document has, plus:",
  "included": ["docs"],
  "docs": {
    "cell": {
      "media_type": "text/markdown; charset=utf-8",
      "content": "# Orders\n\n..."
    },
    "orders_daily@2": {
      "media_type": "text/markdown; charset=utf-8",
      "content": "# Orders daily\n\n..."
    }
  }
}
```

The default `GET /context` (no `include`) never carries page content — only
`declared.docs`, the identity of every declared page (`{target, path,
media_type}`, no bytes) and `declared.include_request`, the affordance
telling an agent how to ask for the rest. `included` is always present
(`[]` on the default document, `["docs"]` once inlined) so an agent can
tell "this server predates docs pages" (the field is absent) from "this
cell just has none" (present, `docs` is `{}`). `?include=docs` on a
docs-less cell is a normal **200** with empty pages, not an error.

Docs content never moves `interface_digest` (the `ETag` on the default
document, `/openapi.json`'s `info.version`, and the mesh manifest's
`context_digest`) — a prose edit is not an interface change. Adding,
removing, or renaming a declared page *is* an interface change (a new
affordance to fetch, or one that's gone), so it does move the digest. The
docs-inlined response carries its own `ETag`
(`"<digest>~docs.<content hash>"`) so caching still works correctly per
variant.

Docs pages fold into the same anti-rot ratchet as the four meaning fields:
they ship in the deploy artifact and move `content_hash` (a docs-only edit
rolls the workload, the same as a schema edit), and `datamk release`'s
meaning digest folds in docs content too, so editing only a page still
draws the "changed meaning without a version bump" warning. See
`docs/adr/0013-long-form-docs-pages.md` for the full design, including the
path-resolution security story (the profile Secret mounts *inside* the
cell directory, so "resolves under the cell dir" alone isn't a safe check).

## Point an agent at it

The document is designed to be an agent's first fetch. Three ways in:

- **Served**: give the agent the base URL and a token; it fetches
  `/context`, then follows `sample_request` for its first working call. The
  back-link headers on every data response let it detect mid-session that
  its cached context went stale.
- **Portable**: `datamk context --out context.json` and paste the file into
  the agent's context (or commit it next to the consuming code). The
  portable artifact stamps `emitted_at` and a digest of the `cell.yaml` it
  came from, and never carries poll telemetry that would be stale the moment
  the file is written. Unlike the served door, it inlines docs pages **by
  default** (a file can't be re-requested with `?include=docs` the way a
  live endpoint can); pass `--no-docs` for identity and fingerprints only.
- **A fleet of cells**: emit a mesh manifest (below) so the agent can find
  *which* cell answers its question before making a single authenticated
  call.

Note what the endpoint deliberately does **not** offer: datamk never accepts
a query from a caller — no filter expressions, no projections, no
natural-language questions on this socket. The grammar is closed. An agent
that needs real SQL should attach the storage plane read-only
(`datamk attach`) and run it with its own engine, credentials, and bill.

## Meaning without rows: `--no-data`

For estates where agents may know what `total_comp` *means* but must never
see rows over HTTP:

```bash
datamk serve -f cell.yaml --no-data
```

The data routes are simply not mounted (404, not 403 — no door, no
implication of one), while `/context` still serves the full declared meaning
plus the aggregate measurements (row counts, date coverage). Row-derived
value lists and example requests are withheld — shipping them would leak a
projection of the withheld rows. Docs pages stay available (`?include=docs`
works exactly as it does with data mounted) — they're author-written prose,
not derived from any row, and are exactly the orientation this mode's agents
need most. Tell consumers where rows actually live via the profile:

```yaml
# profiles/prod.yaml — environment, never cell.yaml
channels:
  - "warehouse: analytics.orders_daily (SELECT with your existing grants)"
```

## Many cells: the mesh manifest

An agent holding one cell's URL can orient on that cell. To tell it which
cells *exist*, emit a static manifest — a document an operator hosts
anywhere (bucket, repo, intranet page), never a registry service:

```bash
# Hand-authored list (any estate):
cat > cells.yaml <<EOF
cells:
  - name: orders
    url: https://orders.data.internal
    auth_hint: orders-token        # a credential NAME the agent resolves in
                                   # its own secret store — never a token
EOF
datamk mesh emit --cells cells.yaml --out mesh.json

# Or a name census over a shared storage prefix (S3/GCS):
datamk mesh emit --store s3://acme/cells \
  --url-template "https://{name}.data.internal" --out mesh.json
```

The emitter fetches each cell's `/context` and copies its description,
export list, and interface digest into the manifest. One owner per string:
nothing beyond `{name, url}` is ever typed into the manifest by hand, so the
cell's own document always wins, and the digest tells a consumer when its
cached copy of a cell's summary has gone stale. Re-run `mesh emit` on
whatever cadence keeps the summaries fresh.

## The trust model, in one paragraph

The document's authority comes from the build. `verify` checks the declared
schema and grain uniqueness against the actual rows on every build, and a
published execution is created only after `verify` passes — so
`status: "verified"` means a machine checked the claims against the data,
recently, and the document can prove when. The prose is the one part the
machine cannot check; the ratchet bounds its drift, and the
`declared`/`observed` split makes sure an agent always knows which kind of
statement it is reading.
