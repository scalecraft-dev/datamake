# ADR 0017 — Definitions: cell-level terms, and `GET /context?terms=`

- **Status:** Accepted — 2026-08-31.
- **Date:** 2026-08-31
- **Deciders:** Datamake team
- **Author:** @scottypate
- **Amends:** ADR 0013 premise (c) (docs never queryable independent of the
  one document) — falsified by a live design-partner use case, below. ADR
  0013 §1's plurality refusal and §2's path rules stand and are reused.

## Motivation

A design partner running a discovered (SQLMesh, ADR 0016) cell attaches
prose per model via `overrides[].docs`. What they actually have to
document is not per-model: metric and business definitions that span
several columns or several models ("net revenue" is `invoice_amount` on one
export less `credit_memo` on another), and concepts tied to no column at
all. Two things are true at once:

1. The existing cell-level `docs:` page (ADR 0013) already *holds* that
   prose, is kept on every `/context/<route>` (`src/context.rs::narrow_to`),
   and the partner never found it — the discovered scaffold (`src/init.rs`)
   emits no `docs:` key and `docs/guides/discover.md` frames docs
   per-export. That is a discoverability defect, fixed in this change.
2. One page is **not** enough, and this is the part that reopens ADR 0013:
   the partner's agent holds a short list of terms it needs defined and
   must get *those terms' definitions and documents* — not every page on
   every call. On a 1,969-model estate, a whole-glossary payload on every
   narrowed request defeats the property `/context/<route>` exists for. ADR
   0013 premise (c) said "if that need appears, it argues for a client-side
   index." A client-side index still requires fetching everything first;
   the need is server-side selection.

Confirmed with the founder: this is **lookup, not search** — the agent
arrives holding a term string (from a column name, a question, a ticket)
and needs a deterministic, cacheable resolution of it. Nothing here ranks,
fuzzes, or embeds.

## 1. Authoring surface

`definitions:` is a new, optional, top-level key in `cell.yaml`. It is
either the list itself or **one** relative path to a file that carries the
identical list under the same key — the file form exists so a large
glossary need not be commingled with the interface. Both at once is a parse
error; a list of files is refused (ADR 0013 §1's plurality rule).

```yaml
cell: invoicing
description: Deployed invoicing models.
docs: docs/cell.md                      # unchanged — the one overview page

definitions:                            # inline …
  - term: net_revenue                   # key: ^[a-z0-9][a-z0-9_.-]*$, ≤64 chars
    aliases: [nr, revenue_net]          # optional, ≤5, same grammar
    description: Invoiced revenue less credit memos. Excludes accruals.   # required
    docs: docs/terms/net_revenue.md     # optional long-form page
    applies_to:                         # optional; empty = cell-wide concept
      - flight_spend@1.invoice_amount   #   route.column
      - margins@2                       #   whole route
```

```yaml
definitions: definitions.yaml           # … or one file, anywhere under the cell dir
```

```yaml
# definitions.yaml — cut/paste-identical to the inline block
definitions:
  - term: net_revenue
    description: …
    docs: docs/terms/net_revenue.md     # resolved against the CELL dir, never this file's dir
```

Validation, all at `CellDef::load` beside `validate_prose`/`docs::validate_all`,
hard error, never truncation (ADR 0013 §3 discipline):

- `term` required, grammar above; `description` required, ≤1000 characters
  (orientation, not a page — long form goes in `docs:`).
- Term and alias names are **unique cell-wide across both sets**,
  compared ASCII-case-insensitively (lookup is case-insensitive, §3).
- ≤256 definitions per cell. ≤5 aliases per term.
- `applies_to` entries are `name@major` or `name@major.column` and must
  resolve to a discoverable export / one of its declared columns — an
  unknown route or column is a hard error naming what exists, the same
  orphan-prose rule ADR 0012 §3 ratchet 2 applies to column descriptions.
  On a discovered cell this runs **after** `config::load` materializes the
  interface (ADR 0016 amendment item 2's placement for `overrides[].docs`).
- `docs:` on a definition and the `definitions:` file path both go through
  `config::docs::resolve_path` unchanged — relative only, canonicalized under
  the cell dir, `profiles/` and `.cell/` denied. The file is capped at 256
  KiB; definition pages count against the existing 64 KiB/page and 256 KiB
  total docs caps (which now genuinely bind — correct).
- Unknown keys in the definitions file are an error, same strictness as
  `cell.yaml`.
- After load, downstream code sees one `Vec<Definition>`; nothing past
  `config::load` knows or cares which form was authored.

**Refused, even if asked:** an `expression:`/SQL/formula field on a term
(the semantic-layer line ADR 0012's Alternatives refused; the one variant of
this proposal that cannot be reshaped), per-export *different* meanings for
one term (those belong on the export's schema record), a glossary shared
across cells (ADR 0013 amendment invariant 1 — a decoupled catalog).

## 2. Document shape — additive, `datamk_context` stays 4

Two new top-level fields on the flat document (ADR 0015), both **always
present**:

```json
{
  "definitions": [
    { "term": "net_revenue", "aliases": ["nr", "revenue_net"],
      "description": "Invoiced revenue less credit memos. Excludes accruals.",
      "applies_to": ["flight_spend@1.invoice_amount", "margins@2"],
      "from": { "description": "cell.yaml" } }
  ],
  "missing_terms": [],
  "definitions_request": "context?terms=<term>[,<term>]&include=docs",
  "docs": [
    { "target": "cell", "source_path": "docs/cell.md", "media_type": "text/markdown; charset=utf-8" },
    { "target": "definition:net_revenue", "source_path": "docs/terms/net_revenue.md",
      "media_type": "text/markdown; charset=utf-8" }
  ],
  "included": []
}
```

- `definitions[]` is the **index** — the advertisement an agent reads before
  it asks. It is not gated behind `include=`: gating a field the document
  claims is always present would be a lie, and an agent recovering from a
  miss needs the vocabulary in the default fetch. `description` ships here
  (it is the term's short form, the analogue of `export.description`).
- A definition's page is a `docs[]` entry with `target: "definition:<term>"`
  — the third target form beside `"cell"` and `name@major`. `:` occurs in
  neither existing form, so the namespace is unambiguous and
  `Published.docs` (a map keyed by target) and `inline_docs` work unchanged.
  Content arrives only under `include=docs`, as for every other page.
- `from` on each definition is `{description: "cell.yaml"}` — definitions
  are authored-only today; the field exists so a future adapter supplying
  terms (dbt metrics) is not a re-meaning.
- `missing_terms` is engine-emitted and always present (`[]` when nothing
  was asked or everything resolved) — the assert-absence discipline
  `included` carries: absent means the server predates this ADR.
- `definitions_request` is the affordance constant beside `include_request`.

## 3. The `terms=` query parameter

```
GET /context?terms=net_revenue,active_customer&include=docs
GET /context/flight_spend@1?terms=nr
datamk context -f cell.yaml --terms net_revenue,active_customer
```

Grammar, closed (ADR 0012 §7 / ADR 0013 §4): `/context` and
`/context/<route>` accept exactly `include` and `terms`; any other parameter
→ 400. `terms` is comma-separated; an empty value, an empty segment, a token
outside `[A-Za-z0-9_.-]`, or more than 64 tokens → 400. Repeated identical
tokens deduplicate. Matching is ASCII-case-insensitive over every term and
alias; a hit resolves to its canonical `term`, and two tokens resolving to
one term yield one entry.

**An unknown term is not an error.** A robot asking for three terms and
getting one wrong must keep the other two, and a 404 would push it back into
N requests — the thing being fixed. The response is **200** with the hits,
and every unmatched token echoed verbatim in `missing_terms`; the default
document carries the full index, so recovery is one retry. Silence would be
ADR 0013 §4's false-confidence failure (`?include=dcos`); 404 stays reserved
for an addressed resource (`/context/<route>`). The **CLI** is the
deliberate asymmetry ADR 0013 §7 already established: `datamk context
--terms` exits non-zero on any unknown term, naming the known ones — a file
written by `--out` cannot be re-requested, and an artifact silently missing
a term the author asked for is a dangling pointer.

What `terms=` narrows: `definitions[]` to the selected terms, and `docs[]`
to the selected terms' `definition:` entries **only** — the cell page and
export pages are dropped, or `include=docs` under a filter would carry every
page again. `exports[]`, `description`, `notes[]` and everything else stay
whole: one schema for every variant, as `narrow_to` already does.

Composition with `/context/<route>`:

- Without `terms=`, the route narrowing keeps a definition iff `applies_to`
  is empty (cell-wide) or names that route or one of its columns, and keeps
  those definitions' pages alongside the cell page and the export's page.
- With `terms=`, the explicit list resolves against the **whole cell**, not
  the route's scope — otherwise `missing_terms` would have to mean two
  different things ("no such term" vs "out of scope for this route"), a
  second false-confidence hole. Exports are still narrowed to the route.

No `/context/definitions/<term>` route. ADR 0012 §4's refusal was of a second
route describing the same exports; a term is a different object, so that
refusal does not auto-apply — but terms are inherently a *set* (a path
segment gives one per request), and `definitions` as a reserved segment in
the route namespace is a word no export could then be named. A query filter
on the one document costs one handler, one auth check, one cache-key space.

No fuzzy, substring, or full-text matching, now or later on `serve`: a
near-miss returning the wrong definition is the confidently-wrong-answer
failure the meaning fields exist to prevent, ranking has no honest `ETag`,
and `docs/guides/context.md`'s rule stands — datamk never accepts a query a
caller composes. `aliases:` is the recall mechanism, as an authored,
collision-checked, cacheable fact. If `missing_terms` fills up in practice,
the answer is a client-side index over the fetched document, not a
server-side matcher.

## 4. Digests and caching

- **`interface_digest`** gains each definition's `term`, `aliases`,
  `applies_to`, and its page's identity (`target`, `source_path`,
  `media_type`) — affordances. A term's `description` text and page content
  stay **out** (ADR 0013 §5: a prose typo must not tell OpenAPI tooling the
  callable surface changed). The distinguishing rule, since column
  `description` text *is* in the projection (ADR 0015 §5): text on an
  export's schema record is interface; a glossary term is a docs-class
  object. Adding, removing, or renaming a term or alias moves the digest —
  that is a real interface change (a new thing to ask for).
- **ETag.** The default and `include=docs` variants are byte-identical to
  before this ADR when `terms=` is absent (mesh.rs copies the default
  verbatim). Under `terms=`, the ETag appends `~terms.<sha12>` over the
  sorted canonical selected terms; under `terms=` **and** `include=docs`,
  the `~docs.<sha12>` suffix is computed over the *selected* pages' sha256s
  in declared order rather than the startup-precomputed bundle. This is the
  codebase's first N-of-M selector and therefore the first hashing on the
  request path — 2^N subsets cannot be precomputed. It is bounded (≤64
  tokens), in-memory over sha256 strings already resident in `AppState`,
  touches no filesystem, store, or DuckDB. Stated here rather than found.
  A subset yields a distinct tag by construction, so a client holding a
  wider variant's tag cannot false-304 into a narrower one.
- **`Published.docs`** carries `definition:<term>` fingerprints beside
  `"cell"` and route keys — computed at `datamk release`, never at load.
- **Deploy artifact / `content_hash`** collects the definitions file (if
  any) and every definition's page into the same dedup-by-path pool
  `CellArtifact::collect` already uses (ADR 0013 §9) — one pass, not a
  parallel one. A definitions-only edit rolls the workload.

## 5. The release ratchet — fan-out, and a bug closed

`release.rs::description_digest` folds, for each supported route, every
definition whose `applies_to` names that route or one of its columns —
sorted by term, hashing `term`, `aliases`, `description`, and page content.
One definition edit can now move several exports' digests; that is correct —
the term *is* part of what those columns mean.

Cell-wide definitions have no export to fan into, and neither does the
cell-level page: `Published.docs` fingerprints are written
(`src/release.rs`) but compared nowhere, so today an edit to the cell page
escapes ADR 0013 amendment invariant #4 entirely. This ADR moves the
business glossary into exactly that unwatched place, so the gap closes in
the same change: `Published.descriptions` gains one `"cell"` entry (route
keys always carry `@major`, so no collision — ADR 0013 §5's own rule)
digesting the cell `description`, the cell page content, and every
definition in canonical order. At the next release a changed `"cell"`
digest draws a warning naming the cell and, where possible, the term — it
cannot say "bump the version" because no version governs the cell; it says
the meaning moved and to review it. Manifests predating this ADR have no
`"cell"` entry and draw no warning on first release.

## 6. Portable, `--no-data`, mesh, OpenAPI

- `datamk context --terms a,b` mirrors `--export` with the identical
  predicate and composes with it. Definition pages inline by default;
  `--no-docs` withholds page content (cell, export, and definition pages)
  while `description` survives — it is the term's short form, which
  `--no-docs` never withheld for exports either.
- Definitions and their pages stay available under `serve --no-data` —
  author prose, not row-derived (ADR 0013 §10).
- `datamk mesh emit` gets nothing; `context_endpoint` still never carries a
  query string.
- `/openapi.json`: `terms` documented on `/context` and `/context/{route}`
  (`style: form, explode: false`, array of strings, `enum` = every term
  then every alias in declared order — the `route` parameter's enumeration
  precedent, generated from the same list the handler matches against);
  `definitions[]`, `missing_terms`, `definitions_request`, and the
  `definition:` target form in the response schema; 400 listed where
  missing. A fixture test binds the parameter to the vocabulary, as ADR
  0013 §8 bound `include`.

## 7. Discoverability fixes shipped alongside

- Both `datamk init` scaffolds gain commented `docs:` and `definitions:`
  lines, framed as "definitions spanning models; concepts tied to no
  column."
- `docs/guides/discover.md`'s docs section leads with the cell page and
  definitions, then the per-override page.
- `docs/guides/context.md` gains a "Definitions" section.

## What was refused

- `/context/definitions/<term>` — §3.
- Fuzzy / substring / full-text / embedding search on `serve` — §3.
- `include=definitions` as a gate on the index — §2.
- `expression:`/SQL on a term; per-export term meanings; cross-cell
  glossary — §1.
- A list of definitions files; a `definitions/` directory convention —
  ADR 0013 §1.
- `applies_to=<route>` as a request selector — the route narrowing already
  does this; add only if a caller demonstrably needs "every term touching X"
  independent of the route document.

## ADR 0013 amendment invariants — held

The definitions file and definition pages are side files in exactly the
sense ADR 0013's amendment governs, and all four conditions hold: they live
in the cell directory under the same allowlist (1); they change in the same
commit as `cell.yaml` (2); they are in `content_hash` (3, §4); they are in
the release meaning digest (4, §5). Any future change that drops one of
these is a violation of ADR 0012 §3, not an extension of this ADR.

## Premises

1. The robot arrives holding a term string — this is lookup, not browse.
2. Definitions are authored; no modeling tool in scope supplies them.
3. ≤256 terms per cell and ≤64 per request cover the largest known estate.

## Falsifiers

1. `missing_terms` fills up in real traffic → aliases aren't carrying the
   vocabulary gap; the answer is a client-side index, not fuzzy matching.
2. A caller needs "every term touching route X" outside the route document
   → `applies_to` becomes a request selector.
3. A term needs different meanings per export → it belongs on the export's
   schema record, not the cell glossary.
