# Semantic models (Apache Ossie)

`semantic_model:` ingests business meaning that goes beyond the contract —
field semantics, synonyms, metric definitions, join relationships — from
[Apache Ossie](https://github.com/apache/ossie) (incubating; formerly Open
Semantic Interchange, "OSI") documents authored outside datamake, in the
modeling repo they already live in. datamake never authors Ossie, never
evaluates a metric, never synthesizes a join from a relationship: it
snapshots the source (`datamk sync`), plan-checks every claim it can against
the built tables without executing anything (`datamk verify`), and serves
the result through the context document, narrowed to what an agent asked
for. `definitions:` (see [ADR 0017](../adr/0017-definitions.md)) is unchanged
and still the home for estate-wide concepts that have no column and no
expression — Ossie requires an `expression` on every field and metric, so
it can't represent those.

## Author it

```yaml
# cell.yaml
semantic_model:
  dir: ../dbt-project/osi        # a directory of Ossie files, or one file
```

```yaml
semantic_model:
  git: https://github.com/acme/semantics.git
  path: osi                      # subpath in the repo; default: repo root
  ref: v1.2.0                    # branch, tag, or commit sha; default: HEAD
```

Exactly one of `dir` or `git`. `dir` is relative to the cell directory and
may leave it — the source of truth lives in the modeling repo, not the
cell — but must not resolve into `profiles/` or `.cell/`. `git` shells out
to the machine's own `git` (your ssh agent/credential helper do auth,
datamake holds no secrets); the URL must be `https://`, `ssh://`, or
`git@host:path`. `datamk release`/`datamk deploy` refuse a `git` source
whose `ref:` is not a 40-hex commit sha matching what was last synced — a
moving branch never ships.

Every `.yaml`/`.yml`/`.json` under the source (dot-directories and
dot-files skipped) must be a valid Ossie document: `version` ∈ `0.1.1`,
`0.2.0.dev0` (every file in one source must agree), and a top-level
`semantic_model:` list. Semantic model names are unique across the walk;
dataset names are unique within their model.

Because `dir:` may leave the cell directory (e.g. `dir: ..`), the walk also
skips any directory named `profiles` or `.cell` at any depth, and never
reads a file named `cell.yaml` — a wide `dir:` must not pull in a sibling
cell's environment config or echo `cell.yaml`'s own top-level keys into an
"not an Ossie document" error.

## Sync

```bash
datamk sync -f cell.yaml
```

Walks the source, merges every file into one document, and writes
`.cell/semantic_model.json` — the only place `verify`, `context`, `serve`,
and the deploy artifact read from; none of them touch the directory or the
network again. `sync` now refreshes *all* external state in one pass: the
`discover:` catalog when present, `semantic_model:` when present, and
errors only when the cell declares neither. The semantic half needs no
profile.

When `semantic_model.git.ref` is a pinned 40-hex commit sha and
`.cell/semantic_model.json` already records that exact commit from the
identical `git`/`path`/`ref`, `sync` reuses the snapshot: no clone, no
fetch, no network access at all — it prints `reusing
.cell/semantic_model.json at <sha> (pinned; nothing to fetch)` and exits 0.
This is what lets a deploy pod with no git credentials run `sync` safely:
CI produces the pinned snapshot with the runner's own creds at build time,
and a commit sha can never resolve to something different later. `datamk
sync --refetch` forces the clone regardless.

## Bind datasets to exports

A dataset binds to an export when its `source`, normalized (quotes
stripped, ASCII-lowercased), equals the export's discovered model name
(SQLMesh FQN), its route key (`name@major`), or its plain name. A dataset
that binds to nothing is kept, marked unbound, and excluded from every
route's context — the same `osi/` can serve every cell cut from one repo. A
dataset bound to two majors of one export name binds to both routes.
Binding is never by dataset name alone.

## Verify

```bash
datamk verify -f cell.yaml -p prod
```

For every bound dataset, without executing a query:

| Claim | Check |
|---|---|
| A field's identity expression (bare column name) | Must be a declared column of the export — recorded `reason: "error"`, lists the declared columns, fails the check. |
| Any other `ANSI_SQL` field expression | `DESCRIBE SELECT <expr> FROM <source>` must succeed — recorded `reason: "error"` quoting DuckDB's message, fails the check. |
| A field with no `ANSI_SQL` variant | Recorded `verified: false, reason: "dialect"` — warned once per dataset, never fails the check. |
| A field expression calling a function DuckDB doesn't have | Recorded `verified: false, reason: "function"` — warned, never fails the check. A foreign-dialect callable authored under `ANSI_SQL` (e.g. BigQuery's `HLL_COUNT.MERGE`) reads this way; a bad *column* still fails it. |
| `primary_key` | Compared to the export's grain as a set. Differs → recorded `"error"`, fails the check. No export grain → recorded `no_grain`. |
| Relationship `from_columns`/`to_columns` | Must be fields or declared columns of their datasets, when both sides are bound; same length. Bad → recorded `"error"`, fails the check. Unbound side → recorded `unbound`. |
| Metric expression | `DESCRIBE SELECT <expr> FROM` a cross product of every bound dataset of the model — identifier/type resolution only, no join synthesized, no rows read. A referenced dataset that's unbound → `unverified: unbound`; no `ANSI_SQL` variant → `unverified: dialect`; an unqualified expression over more than one bound dataset → `unverified: ambiguous`; a missing function → `unverified: function`; DuckDB genuinely rejects it → `"error"`, fails the check. |

A false claim is recorded, never bailed out on mid-pass (follow-up 8):
every dataset, relationship, and metric is still checked, the full summary
block prints with every failure `✗`-marked, and only then does `verify`
exit non-zero, naming every failure it found — not just the first one a
15-minute bind pass happened to reach.

Results land in `.cell/semantic_check.json`, keyed `model/dataset@route`
(one entry per bound route — a dataset bound to two majors gets two
entries), stamped with `cell_yaml_digest`, `profile`, `checked_at`, and the
synced content's own `content_sha256` — written only when every check in
the pass passed.

### `--semantic-only`

```bash
datamk verify -f cell.yaml -p prod --semantic-only
```

Runs only the checks above and writes only `.cell/semantic_check.json` —
skips the declared-schema type/grain checks, the column census, the live
half of the `contract: supported` description lint, and
`.cell/source_check.json`/`.cell/source_descriptions.json` entirely.
Refused on a cell with no `semantic_model:` ("nothing to check").

Exists because a full live verify over a large discovered estate binds
*every* declared source before checking anything — on a 41-export SQLMesh
estate, that bind pass alone took ~15 minutes, almost none of it needed
for the Ossie checks, which are `DESCRIBE` only. `--semantic-only` still
binds every declared source when the cell has a bound export (a dataset
can bind to a bound export's live view), but via a schema-only probe: the
identical read wrapped so the connector returns a correctly-typed *empty*
result instead of the whole view — zero rows can never trip BigQuery's
`EXPORT DATA` staging escalation, so that escalation never fires. An
incremental source still stages for real (`--semantic-only` does not
touch watermark logic), and a materialized (unbound) export's checks run
against the already-built lake table either way, untouched by the flag.

## The four doors

| Door | Returns |
|---|---|
| `GET /context` / `datamk context` | `semantic_models[]` — name, description, file, dataset/bound/metric counts. Always present, `[]` without a source. |
| `GET /context/<route>` | `exports[].semantic[]` — every dataset bound to the route, with fields, `ai_context`, per-field verification, the model's relationships touching it, and metrics (in full when every referenced dataset binds to this route, else as a `metric_refs` pointer). |
| `GET /context?model=<name>` / `datamk context --model <name>` | One semantic model in full (`semantic_model`), composable with `terms`. A whole-cell view — mutually exclusive with `--export`/route narrowing. |
| `GET /context?terms=<t>` / `datamk context --terms <t>` | Ossie dataset, field, and metric names, plus `ai_context.synonyms`, join the `definitions:` lookup index — hits land in `semantic_matches[]`, each naming its model; collisions across models return every hit. |

`datamk mcp` adds one resource per model: `datamk://<mount>/semantic/<name>`,
the same document `?model=` returns.

The top-level `semantic` block (`synced_at`, `content_sha256`, `resolved`,
`checked_at`) is a measurement, outside every digest. `interface_digest`
folds in lookup keys only — sorted model, dataset, field, and metric names,
plus addressable synonyms — never prose, never verification: a description
edit doesn't move an agent's cache; a new addressable name does.

## Staleness

Same regime as `discover:`'s sidecar (ADR 0016 §5), applied to the Ossie
half:

- **`cell.yaml` changed since the last sync.** `serve` refuses to start when
  `semantic_model:` is declared and `.cell/semantic_model.json` is missing
  or its digest no longer matches — run `datamk sync` and restart.
  `datamk context` warns on stderr, exits 0, and notes it in the document:
  *"semantic_model: declared but no fresh snapshot — definitions from Ossie
  are absent; run `datamk sync`"*.
- **Source drift.** `datamk context` (never `serve`) re-walks a `dir:`
  source and compares its content hash to the synced record; a mismatch is
  a stderr warning plus a `notes[]` entry — the document still describes
  the synced snapshot. A `git:` source is never re-checked over the
  network; the note says how long it's been since `synced_at`.
- **`datamk verify`'s check.** `semantic.checked_at` is present only while a
  fresh `.cell/semantic_check.json` stands; an unfresh one is silently
  omitted, never served as if it still applied.

## Recommended `osi/` layout (SQLMesh estates)

- One file per semantic model, one model per schema.
- `source:` is the quoted fully-qualified name DuckDB would need:
  `"dw"."invoice"."flight_spend"`.
- `version: 0.1.1` (the stable Ossie release; pin it explicitly rather than
  tracking `0.2.0.dev0`, which is still moving).
- Redeclare shared dimensions (e.g. `order_date`, `region`) per model that
  needs them — Ossie has no cross-model include; repetition here is
  cheaper than a shared-fragment system that would need its own
  invalidation story.
- Cross-estate concepts that have no column and no expression (a business
  term spanning several models, a policy, a definition with nothing to
  verify against) stay in `definitions:` — that's the ADR 0017 line this
  ADR does not move.

## Gotchas

- Ossie's own validator can pass an expression against a column that
  doesn't exist; `datamk verify`'s `DESCRIBE` against the built table
  can't be fooled the same way — a bad claim fails `verify`/`run`'s
  auto-verify exactly like a bad declared column.
- A metric is never evaluated and a relationship never joined by datamake,
  anywhere, under any flag. There is no `/metrics/<name>` route.
- `custom_extensions[].data` is opaque to datamake for every vendor — never
  parsed, always passed through byte for byte, uninterpreted. Nothing
  datamake-specific lives inside an Ossie document (see "Refused" in
  [ADR 0018](../adr/0018-ossie-ingest.md)): no datamake-defined `ai_context`
  key, no `vendor_name: DATAMAKE` payload. `vendor_name` is a closed enum
  (`COMMON`, `SNOWFLAKE`, `SALESFORCE`, `DBT`, `DATABRICKS`, `GOODDATA`) in
  Ossie 0.1.1 — `DATAMAKE` is valid Ossie only under `0.2.0.dev0`, which
  makes it free-form.
- A synonym outside `[A-Za-z0-9_.-]{1,64}` is kept in the document's prose
  but never addressable via `terms=` — `datamk sync` warns about it once.

Design rationale: [ADR 0018](../adr/0018-ossie-ingest.md). See also
[context.md](context.md) for the document shape and [discover.md](discover.md)
for how this composes with a discovered interface.
