# ADR 0013 — Long-form docs pages: `docs:` and `GET /context?include=docs`

- **Status:** Accepted — implemented 2026-08-08 on `docs-long-form-pages`.
- **Date:** 2026-08-08
- **Deciders:** Datamake team
- **Author:** @scottypate

## Motivation

`../dplat-datamake/flight-spend/cell.yaml` carries roughly 25 lines of real
explanation — which column is invoiced revenue, what `flight_id = -1` means,
why `budget` is prior-spend-adjusted rather than period-scoped — written as
YAML comments `serde` discards. ADR 0012 §3 gave the author exactly four
meaning fields (`cell.description`, `export.description`, per-column
`description`/`unit`), each rationed to "not machine-derivable AND wrongness
produces a confidently wrong number," each capped at one line or a couple of
sentences. That rationing was deliberate and remains correct for the fields
it governs — but it leaves nowhere for prose that is legitimately long: the
one-paragraph explanation of *why* `budget` behaves the way it does, worth
writing once, that doesn't fit two sentences without losing the reason a
wrong guess happens in the first place.

Confirmed with the founder before writing code: the need is a home for
long-form prose an **author writes**, not an importer for prose that exists
somewhere else. No CMS, no wiki sync, no doc-generator ingestion — a file in
the cell directory, versioned with the schema it explains.

## The central decision: one document, one route

Docs are delivered **inline in the context document, via one request**:
`GET /context?include=docs`. There is **no** `/docs/:name` route.

ADR 0012 §4 renamed `/interface` to `/context` specifically to avoid owning
drift between two routes describing the same exports — "There is still
exactly one document; we do not own drift between two routes describing the
same exports." A `/docs/:name` endpoint is exactly the second door that
decision refused, wearing a different name. Everything ADR 0012 built to
keep the document honest — the shared visibility-filtered route list, the
`declared`/`observed` split, the interface digest, the caching story — would
need a parallel, independently-maintained implementation for a docs route.
Query-parameter gating on the existing route costs none of that: one
handler, one cache key space, one auth check, one place a private export's
docs page can leak from (nowhere, by construction, since it derives from the
same route list `context::declared` already filters).

## 1. Authoring surface

`docs:` at exactly two levels — the cell and each export — one relative path
per entry, additive to `description`, never a replacement:

```yaml
cell: orders
description: Daily order revenue by region.
docs: docs/overview.md
interface:
  - name: orders_daily
    version: 2.1.0
    description: One row per (order_date, region) with summed revenue.
    docs: docs/orders_daily.md
```

**Refused:** lists of paths, glob patterns, per-column docs, docs on
sources/transforms/profiles, an implicit filename convention (`docs/<export
name>.md`), remote URLs (`https://`, `s3://`). Each of these reopens a
question ADR 0012 §3 already closed for the four meaning fields — plurality
invites reordering-as-drift, an implicit convention is a second parser to
keep in sync with the first, and a remote URL puts a network fetch (and a
new trust boundary) on either the config-load path or the request path,
neither of which this feature is allowed to touch.

## 2. Path resolution — security-critical

"Must resolve under the cell directory" is **not sufficient**, and shipping
it would open a credential-disclosure hole. The profile Secret mounts at
`/cell/profiles` — *inside* the cell directory
(`PROFILE_MOUNT`, `src/deploy/targets/kubernetes/render.rs`). `docs:
profiles/prod.yaml` needs no `..` at all to expose `s3.key_id`/`s3.secret`:
it is a plain relative path that resolves under the cell dir by the naive
rule. `../etc/datamk/principals.json` is the bearer-token → roles map the
same reasoning reaches with one more `..`.

The allowlist actually implemented (`src/config/docs.rs::resolve_path`):

1. **Relative only** — an absolute path is rejected outright, no
   canonicalization needed to know that.
2. **Canonicalize, then require the result under the canonicalized cell
   directory.** Canonicalizing (not just joining) resolves symlinks, so a
   symlink planted inside the cell dir that points outside it is caught by
   the same check that catches `..` — one control, not two.
3. **Explicitly deny resolution into `profiles/` or `.cell/`.** This is the
   security-load-bearing step the naive "must be under the cell dir" rule
   is missing: both directories are legitimately *under* the cell directory
   (that's exactly why the naive rule doesn't catch them) and both are
   engine/environment-owned, never author-owned prose.
4. **Require valid UTF-8** — the context document is JSON; a docs page that
   isn't valid UTF-8 text cannot be represented in it, so this is caught at
   validation time, not serialization time.

Mandatory tests (`src/config/docs.rs`): `docs: profiles/prod.yaml` rejected;
`docs: ../etc/datamk/principals.json` rejected; a symlink escape rejected;
an absolute path rejected. All four exist and pass.

## 3. Caps — load-bearing, fail loud, never truncate

64 KiB per page; 256 KiB total per cell. Enforced at `CellDef::load`, next
to `validate_prose` — the same fail-fast discipline the meaning-field length
caps already have, and the same discipline `load_principals` uses for the
profile's `principals:` file: an unreadable, oversized, empty, or non-UTF-8
page is a hard error, not a warning, not a truncation.

The number is not arbitrary: the Kubernetes ConfigMap that delivers cell
content to a deployed Server is capped at 1 MiB by the API server, and that
budget is shared with `cell.yaml`, every transform's SQL, and
`published.json` (`src/deploy/targets/kubernetes/render.rs`). Truncating
prose to fit is a silent meaning change — an agent reading a truncated page
has no way to know the sentence that would have corrected its guess was cut
off mid-word. A hard error at author time, naming the exact byte counts and
the caps, is the only acceptable failure mode.

Pages are read at config-load/serve-startup time into memory (`Arc<str>` in
`serve`'s cache); **handlers never touch the filesystem** (ADR 0012 §5's
same rule for probes and provenance). The mount is immutable for a pod's
lifetime, so unlike the swap-time probes, docs content needs no
poller-driven re-read — it is computed once, at `build_state`, and never
touched again.

## 4. The `include` query parameter

`context_doc` (`src/serve/mod.rs`) had **no** query extractor before this
change — any query string was silently ignored. `?include=dcos` (a typo)
would have returned `content: null` with no error, which an agent reads as
"this cell has no docs" — the exact false-confidence failure
`validate_params` already exists to kill on the data door (ADR 0012 §7).
This ADR closes the same hole on `/context`.

Grammar: `include=<comma-separated closed set>`, vocabulary `{docs}` today
(`serve::INCLUDE_SECTIONS`, the same constant `openapi::generate` documents
the parameter from, so the two can never drift).

- Any parameter other than `include` → 400.
- An unrecognized `include` token, an empty value (`?include=`), or a
  trailing/empty comma segment (`?include=docs,`) → 400.
- A repeated identical token is accepted (deduplicated).
- `?include=docs` on a cell with **no** declared docs → **200**, `docs: {}`,
  `included: ["docs"]`. Not an error — the request was well-formed and
  answered truthfully; there is simply nothing to return.

Implementation note: the handler takes `Query<Vec<(String, String)>>`, not
`Query<HashMap<String, String>>` — the latter silently collapses repeated
keys before validation ever sees them, which would make "repeated identical
token accepted" true by accident and "two different values for the same
key" impossible to reject on purpose.

## 5. Document shape — three places, never collapsed

- **`declared.docs`** — identity only: `{target, path, media_type}` per
  entry. `target` is `"cell"` or the route key (`name@major`) — route keys
  always carry `@major`, so no collision with the literal string `"cell"`.
  Always present (`[]` when none), alongside an affordance field
  `declared.include_request: "/context?include=docs"` — the same pattern
  `declared.exports[].query` already establishes (an affordance living
  inside `declared`). **No content-derived value here** — no sha256, no
  bytes. `interface_digest` serializes the whole `Declared` struct, and that
  digest is also `/openapi.json`'s `info.version` and the mesh manifest's
  `context_digest`; a prose typo must not tell generic OpenAPI tooling the
  callable surface changed. Because identity (path, target) *is* part of
  `Declared`, adding, removing, or renaming a page legitimately does move
  the digest — that is a real interface change (a new affordance to fetch).
- **`observed.docs`** — `{target: {sha256, bytes}}`. A machine fact,
  **not** in the interface digest. Computed at `datamk release` time
  (`src/release.rs`) from the same declared pages, and carried through
  `published.json` (`manifest::Published::docs`) into both `serve`'s
  startup read and `datamk context`'s emit — never recomputed at
  config-load time. That distinction matters: `observed` is `Option<...>`
  and must stay `null` on a cell nothing has been built against
  (`context.rs`'s `draft_document_asserts_unbuilt_status_positively`);
  computing a fingerprint at load time would populate `observed.docs` for
  every cell that merely *declares* `docs:`, regardless of whether anything
  ever built or released it, which is exactly the claim-as-measurement
  confusion ADR 0012 §2 forbids.
- **Top-level `docs`** — `{target: {media_type, content}}`. Present
  **only** under `include=docs`. Not under `declared` (see the digest
  reasoning above) and never under `observed` — author bytes are a claim,
  not something the machine measured, and an agent that reads `observed` as
  "measured" must never find author prose there.
- **Top-level `included`** — engine-emitted, **always present**: `[]` on
  the default variant, `["docs"]` when docs were inlined. This is the
  new-agent/old-server signal: an old binary returns 200 with the field
  *absent* (the JSON key doesn't exist), letting a client distinguish
  "this server predates the feature" from "I asked, and this cell has
  none" — the exact assert-absence discipline ADR 0012 §2 already applies
  to `observed`/`grain_verified`.

Every new field is additive, so `DATAMK_CONTEXT_VERSION` does not bump
(ADR 0012 §2: additive changes don't bump; only removal, rename, or
re-meaning does).

## 6. Caching

- The default `GET /context` `ETag` is unchanged: `"<interface_digest>"`,
  byte-identical to before this ADR. That matters beyond habit: the mesh
  emitter (`src/mesh.rs`) copies this ETag verbatim into the manifest's
  `context_digest`, and it never requests `?include=docs` (§9) — keeping
  the plain variant's ETag untouched makes this a complete non-event for
  every existing consumer.
- The docs variant's `ETag` is `"<interface_digest>~docs.<bundle sha12>"`,
  where the bundle sha is a hash over every page's sha256 in declared
  order, truncated to 12 hex chars (the same truncation convention
  `deploy/targets/kubernetes/render.rs`'s `content_hash_short` already
  uses for the ConfigMap name). Precomputed once at `serve` startup
  (`AppState::docs_bundle_sha12`), never on the request path.
- `X-Datamk-Context-Digest` (the data-route back-link header),
  `/openapi.json`'s `info.version`, and the mesh manifest's
  `context_digest` all keep the **plain** digest, always. The rule: the
  interface digest names the interface; the `ETag` names a representation
  of it — a response can have more than one representation without the
  thing it represents changing.
- No `Vary` header for this. `Vary` names *request headers* the response
  varies by; the query string is already part of the cache key by
  definition, so a `Vary: include` would be meaningless (and `include`
  isn't a header). Not adding it isn't an oversight to flag later.
- The pre-existing exact-match `If-None-Match` check now fixes a real bug
  by construction: before variants existed, there was only one `ETag` to
  match against, so this was moot; now, a client holding a cached *plain*
  `ETag` that requests `?include=docs` will not exact-match the docs
  variant's `ETag` and correctly gets a fresh 200 with content, never a
  false 304 that would silently hide the docs section from a client that
  never actually had it cached.
- `Cache-Control: private` added to both `/context` and `/openapi.json`
  (previously neither carried it). `authorize()` is all-or-nothing
  (default-deny, bearer-token roles); a shared/intermediate cache keyed on
  URI alone — with no notion of the `Authorization` header — could
  otherwise serve a cached 200 from one caller's authorized request to a
  different, tokenless caller. Small and adjacent to this change, but
  worth taking while in this file rather than filing it as a separate
  finding.

## 7. Portable (`datamk context`) — inlines by default

The one deliberate asymmetry with the served door: `datamk context` inlines
docs content **by default**; `--no-docs` withholds it, emitting identity and
fingerprints only (mirroring `serve --no-data`'s existing withholding
idiom — a negative flag that removes content a door otherwise carries).

The reasoning is not symmetry for its own sake: a served request can always
be repeated with `?include=docs` if the first response omitted content. A
portable artifact cannot — `datamk context --out context.json` produces a
file someone pastes into an agent's context or commits next to consuming
code, and a file with `docs: null` pointing at a path the reader doesn't
have filesystem access to is a dangling pointer, not a safe default.
`included` is truthful either way (`["docs"]` or `[]`), so a consumer of the
file never needs to know which door — served or portable — produced it.

`datamk mesh emit` gets **nothing** from this feature: no docs content, no
fingerprints, no flag. The manifest's job is routing summaries copied from
each cell's own `/context` (ADR 0012 §6); it was never the docs door, and
nothing here changes that. `src/mesh.rs::context_endpoint` — the pure
helper the fetch URL is built from — is unit-tested to never carry a query
string, so this is guaranteed by construction rather than by nobody having
typed one.

## 8. OpenAPI — fixing an honesty bug in the same change

`openapi::generate` emitted **only** data paths before this ADR: `/`,
`/context`, and `/openapi.json` appeared nowhere in the generated spec, and
under `--no-data` the document served `"paths": {}` while three routes were
demonstrably live (`/`, `/context`, `/openapi.json` all still answered
requests). This was a pre-existing bug, not introduced by this feature, but
this feature is the first to need `/context` documented at all (the new
`include` parameter), so it is fixed in the same change rather than filed
separately: meta path items (`/`, `/context`, `/openapi.json`) are now
**always** emitted; data path items stay gated on the discoverable route
list exactly as before (empty under `--no-data`, since those affordances
genuinely don't exist there).

`/context`'s `include` parameter is documented with `style: form, explode:
false` and `schema: {type: array, items: {type: string, enum: ["docs"]}}`,
generated from `serve::INCLUDE_SECTIONS` — the identical constant
`validate_include` enforces against, so a change to the vocabulary in one
place is a compile-visible change in the other, and a fixture test
(`context_include_param_is_generated_from_the_shared_vocabulary`) binds the
two the same way ADR 0012 §7's `context_query_block_claims_match_the_
enforced_grammar` binds the data query grammar. `/context`'s responses
document 200/304/400/401/403; `/openapi.json`'s document 200/401/403; `/`'s
documents 200.

## 9. Ratchet and delivery follow-through — both mandatory

Two existing mechanisms had to be extended, or the feature would ship with
a gap on day one rather than earn one over time:

1. **`verify`'s promotion lint** (`src/verify.rs::check_supported_have_
   descriptions`) — a `contract: supported` export still requires a
   non-empty `description`; a `docs:` page does not satisfy it. The
   message was extended in place (not duplicated into a second check) to
   say so explicitly: *"a `docs:` page does not satisfy it ... Agents read
   `description` before they fetch a page."* Setting both fields is
   correct and expected — the lint only fires when `description` alone is
   empty.
2. **The deploy artifact** (`src/deploy/artifact.rs::CellArtifact::collect`
   + `content_hash`) now collects every declared docs page — cell-level
   and every export's, deduplicated by relative path before reading, so
   the same page named by two `docs:` fields is fetched once — and folds
   their bytes into `content_hash` alongside `cell.yaml` and the transform
   SQL. Before this, `content_hash` covered only `cell.yaml`/SQL/
   `published.json`: a deploy that changed *only* prose would produce an
   identical hash, the ConfigMap name
   (`deploy/targets/kubernetes/render.rs::configmap_name`) would not
   change, and the running workload would never roll — serving stale
   prose indefinitely behind a content-addressed name that no longer
   matched its content. `configmap_key`'s existing sanitization-collision
   guard (`/` → `_`) now also has to consider docs paths; deduplicating by
   relative path in `collect` (rather than only in the Kubernetes render
   layer) means the same file referenced twice never reaches that guard
   as two entries to begin with.
3. **`release.rs::description_digest`** now folds in a route's docs page
   content (when it has one) alongside the existing description/unit/
   per-column-description inputs. Without this, editing only a docs
   page's prose — leaving `description` and the version untouched — would
   silently escape the existing "changed meaning without a version bump"
   warning ADR 0012 §3's ratchet check 3 already fires for description
   edits. Docs content is meaning; the digest that tracks meaning has to
   track it too.

## 10. `--no-data`

Docs stay available under `serve --no-data`. The reasoning is not new, but
it is stated here so it is not re-litigated later: the values `--no-data`
withholds (`observed.exports[].values`, `example_request`) are row-derived —
shipping them from a mode whose entire point is that rows stay put would
exfiltrate a projection of the withheld rows. Docs prose is author-written
text, not derived from any row; withholding it under `--no-data` would
remove exactly the orientation a regulated-estate customer running
`--no-data` needs most (ADR 0012 §1's "definitions-only" case: agents may
know what a column *means* but must never see rows).

## 11. Security posture — prompt injection

Restating ADR 0012 §8's stance, extended to the larger surface this feature
adds: cell prose is the first place untrusted author text lands in a
trusted agent context, and docs pages are, byte for byte, a much larger
instance of that same surface than the one-line/two-sentence meaning
fields. Nothing here is a new mitigation *for* prompt injection — the caps
(§3) bound the size of the surface, and the fact that docs content ships
only opt-in, on the served door (`?include=docs`, never the default) and
never at all through the mesh manifest (§7), bounds how far it travels
uninvited. Sanitizing the text is explicitly **not** treated as a
mitigation — there is no sanitization step, because escaping characters
does nothing to the semantic content an LLM reads. Any future aggregator
that fans multiple cells' documents into one agent's context must stamp
every string with its origin cell, exactly as ADR 0012 §8 already commits
to for the mesh case.

## What was refused

- **`/docs/:name` as a second route.** The central decision, above — ADR
  0012 already named this failure mode and refused it once.
- **Per-column docs.** The four meaning fields already cover per-column
  orientation (`description`, `unit`); a long-form page per column
  multiplies the authoring surface by the schema's width for a need that
  hasn't shown up, and ADR 0012 §3's rationing rule exists precisely to
  keep the meaning surface narrow.
- **Remote fetch-at-serve-time** (a `docs: https://...` or `s3://...`
  URL). This is exactly the decoupled-catalog shape §12 below explains the
  anti-rot rule forbids: a write path, a lifecycle, and a set of
  permissions independent of the cell's own commit. It would also put a
  network fetch on either the config-load path (which every command
  exercises, including offline ones like `verify`) or the request path
  (which ADR 0012 §5 already forbids for anything but the swap-time
  probes).
- **`docs:` satisfying the `contract: supported` description lint.** §9,
  above.
- **Lists of paths / globs.** One relative path per level keeps the
  authoring surface a fixed, small shape; a list reopens ordering and
  "which one is authoritative" questions a single path never has to
  answer.

## Amendment to ADR 0012 §3

ADR 0012 §3's ratchet states plainly: *"Prose lives on the `Export` and
nowhere else — no side files, no annotation store, no out-of-band metadata
writes."* Docs pages are, honestly, side files. This ADR amends that rule
rather than pretending it doesn't apply.

The amendment is narrow, and the reason it is acceptable is stated as an
invariant, not an aside: the rule existed to forbid a **decoupled catalog**
— prose with an independent lifecycle, an independent write path, and
independent permissions, able to rot out of sync with the schema it
describes because nothing ties its update to the schema's. A docs page does
not have that shape, *provided every one of the following holds*:

1. It lives in the cell directory, resolved and validated against that
   directory alone (§2) — no external store, no separate write API.
2. It is committed in the same commit as the schema it describes — there is
   no tooling that writes a docs page independently of editing `cell.yaml`.
3. It is collected into the deploy artifact and folded into `content_hash`
   (§9) — a docs-only change produces a new, distinct deploy artifact, so
   "the docs are stale relative to what's deployed" cannot silently persist
   across a deploy the way it could for a file the artifact never touched.
4. It is folded into the release-time meaning digest (§9) — a docs-only
   edit draws the same "changed meaning without a version bump" warning a
   description edit does, so the ratchet's drift-detection reaches prose
   wherever it lives, not just the two fields ADR 0012 §3 originally named.

**If any one of these four does not hold — in this implementation or a
future change to it — the ADR 0012 §3 rule is violated, not amended.** That
is the invariant this ADR records, not a one-time justification: a change
that, say, let `datamk context --out` write docs content back into a
different file, or let a docs page live outside the cell directory, or
skipped folding docs into `content_hash` for a "simpler" artifact path,
would reopen exactly the decoupled-catalog failure mode ADR 0012 §3 was
written to forbid, wearing this ADR's name as cover.

## Consequences and risks

- Docs prose is unverifiable by construction, exactly like the four
  meaning fields before it — the ratchet (§9) bounds drift, it does not
  eliminate it. A docs page can still say something false about the data;
  nothing here checks that claim against a row, and nothing could without
  becoming a semantic layer (the thing ADR 0012's Alternatives section
  already refused).
- `/context` now 400s on a query string it previously ignored silently —
  a route-shape change for any existing caller that happened to send an
  unrelated query parameter to `/context` and got a 200 back before. Named
  explicitly here (and in the PR's release notes) rather than treated as
  an implementation detail, following the same discipline ADR 0012 §7
  used for its own unknown-query-param behavior change on the data door.
- The per-page and total caps are a real ceiling on how much an author can
  explain in one page — deliberately. An author who needs more than 64 KiB
  to explain one export has a design problem the cap is meant to surface,
  not a limit to work around by splitting content across an implicit
  convention this ADR already refused (§1).

## Premises

This decision holds while: (a) the docs feature stays additive-only to the
existing meaning fields — falsified if `description` and `docs:` start
diverging in practice (an author writing contradictory claims in each),
which would argue for merging them into one field rather than keeping two;
(b) the 1 MiB Kubernetes ConfigMap ceiling remains the binding constraint on
total cell content size — if the artifact delivery mechanism changes (a
different target than Kubernetes, or a raised ConfigMap-equivalent limit),
the specific byte caps in §3 should be revisited, though the *shape* of the
constraint (a hard cap, never truncation) should not be; (c) no design
partner needs docs content to be queryable or searchable independent of the
one document a cell already serves — if that need appears, it argues for a
client-side index over fetched context documents (the aggregator ADR 0012
§6 already sketches), not a new server-side surface on `serve`.
