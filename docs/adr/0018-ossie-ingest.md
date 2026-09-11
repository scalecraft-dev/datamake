# ADR 0018 — Apache Ossie ingest: `semantic_model:` from a directory or a git ref

- **Status:** Accepted — 2026-09-10.
- **Date:** 2026-09-10
- **Deciders:** Datamake team
- **Author:** @scottypate
- **Depends on:** ADR 0012 (context document), ADR 0015 (provenance),
  ADR 0016 (external state, the sidecar regime), ADR 0017 (`definitions:`,
  unchanged by this ADR).

## Decision, in one paragraph

datamake grows no semantic vocabulary of its own. Business meaning that
goes beyond the contract — field semantics, synonyms, metric definitions,
join relationships — is authored in **Apache Ossie** (incubating; formerly
Open Semantic Interchange, "OSI"; `https://github.com/apache/ossie`) and
**ingested**: `cell.yaml` names where the Ossie documents live, `datamk
sync` snapshots them, `datamk verify` checks every claim it can against
the built tables without executing anything, and the context document
serves the result narrowed to what an agent asked for. datamake never
authors Ossie, never evaluates an Ossie metric, and never synthesizes a
join from an Ossie relationship. `definitions:` (ADR 0017) is untouched:
it remains the home of estate-wide concepts that have no column and no
expression, which Ossie cannot represent (every Ossie field and metric
requires an `expression`).

## Why ingest rather than emit or author (round-2 evidence)

Ossie's own validator (`validation/validate.py`, jsonschema + sqlglot)
passes `SUM(orders.amountttt)` against a column that does not exist.
DuckDB `PREPARE`/`DESCRIBE` against the built table rejects it without
reading a row. Ossie's spec says "definitions travel; trust does not";
datamake is the thing that supplies the trust. A parse-checked,
provenance-labelled, never-executed expression is the same category as
SQLMesh `overrides[].docs` (foreign-authored, engine-checked prose), not
the semantic-layer line ADR 0012 refused. What ADR 0012 refused was
*unexecuted, unverifiable* SQL; this ADR makes it verifiable.

## 1. Authoring surface

One optional top-level key. A typed map with exactly one of `dir` or
`git`; both, neither, or an unknown field is a parse error. Never a
scheme string (`git+https://…//osi@ref`): `@` collides with URL userinfo
and would leak a token into an error message.

```yaml
semantic_model:
  dir: ../dbt-project/osi        # a directory of Ossie files, or one file
```

```yaml
semantic_model:
  git: https://github.com/acme/semantics.git
  path: osi                      # subpath in the repo; default: repo root
  ref: v1.2.0                    # branch, tag, or commit sha; default: HEAD
```

`dir` is relative to the cell directory and **may leave it** (the source
of truth lives in the modeling repo, not the cell). Absolute paths are
refused. It must not resolve into `profiles/` or `.cell/`. This is not an
ADR 0013 side file (conditions 1 and 2 fail by design); it is **external
state under ADR 0016's regime**: snapshotted at `sync`, stamped, staleness
gated. The escape from `resolve_path`'s containment is a separate
function, `ossie::source::resolve_dir`, documented at its call site.

## 2. What is an Ossie file

By shape, never by filename. Walk the path recursively; read every
`.yaml`, `.yml`, `.json`; skip dot-directories and dot-files; sort by
relative path so the merged content digest is stable. Each file must be:

1. A parseable YAML/JSON document — else a hard error naming the file.
2. A map with `version` (string) and `semantic_model` (list): the spec's
   own `required` list. A map with `ontology:` and no `semantic_model` is
   the 0.2 draft's separate ontology document: refused with that name. A
   map with neither: error naming the file and the top-level keys it did
   have (so `semantic_models:` is obvious).
3. `version` ∈ {`0.1.1`, `0.2.0.dev0`}. Every file in one source must
   agree. Strict deserialization against the vendored spec: unknown keys
   are errors, except inside `ai_context`'s object form (spec:
   `additionalProperties: true`), whose remainder is carried uninterpreted.
   `custom_extensions[].data` is a JSON string; it is opaque to datamake
   for every vendor, always passed through byte-for-byte, never parsed
   ("Refused" below). `vendor_name` is a closed enum in `osi-0.1.1-rc1`'s
   schema with no `DATAMAKE` member — `0.2.0.dev0` makes it free-form, but
   datamake never enforces the enum either way.

Merge: `semantic_model[]` lists concatenate in file order. Semantic model
names are unique across the walk (error naming both files). Dataset names
are unique within their model (the spec's rule; nothing cross-model).
Each model records its source file. Caps: 2048 files, 4 MiB per file,
32 MiB merged; symlinks are canonicalized and must resolve under the
walk root.

## 3. Git fetch

Shell out to the machine's `git` — the user's ssh agent and credential
helper do auth, datamake holds no secrets. Never the `git2` crate.
Hardening, non-negotiable:

- URL must start with `https://` or `ssh://`, or be `git@host:path` (scp
  form). Anything else — `ext::`, `file://`, `git://`, a leading `-` — is
  refused before `git` is invoked. Arguments are an explicit `Vec`, never a
  joined string; the URL follows `--`.
- Per-invocation config: `-c protocol.allow=never -c
  protocol.https.allow=always -c protocol.ssh.allow=always -c
  core.hooksPath=/dev/null`. Environment: `GIT_TERMINAL_PROMPT=0`,
  `GIT_ASKPASS=/bin/false`, `GIT_SSH_COMMAND=ssh -o BatchMode=yes`,
  `GIT_LFS_SKIP_SMUDGE=1`. Never `--recurse-submodules`. Hard timeout.
- Shallow, single-ref: `git clone --depth 1 --single-branch [--branch
  <ref>] --filter=blob:none --no-checkout`, then `sparse-checkout set
  <path>` and `checkout`. A 40-hex `ref` is fetched by sha (`fetch --depth
  1 origin <sha>`; falls back to a full fetch if the server refuses).
- Cache under `.cell/semantic/<sha256(url)[..16]>/`, gitignored (the
  `.cell/attach/` precedent). The subpath is canonicalized and checked
  under the checkout root.
- The resolved commit (`git rev-parse HEAD`) is recorded. `datamk release`
  and `datamk deploy` refuse a record whose `ref` is not a 40-hex sha
  written in `cell.yaml` — a moving branch never ships.

Tests exercise the fetch against a local bare repository through an
internal `allow_local` option that `cell.yaml` cannot set.

## 4. The sidecar: `.cell/semantic_model.json`

Written by `datamk sync`, which now refreshes *all* external state: the
`discover:` catalog when present, the `semantic_model:` source when
present, and errors only when the cell declares neither. `sync` needs no
profile for the semantic part.

```json
{
  "datamk_version": "…",
  "cell_yaml_digest": "…",
  "synced_at": "2026-09-10T14:02:00Z",
  "source": { "git": "…", "path": "osi", "ref": "v1.2.0" },
  "resolved": { "commit": "4f2a91c…" }            // or { "dir": "/abs/path" }
  "content_sha256": "…",                          // over the sorted file bytes
  "files": ["invoice.yaml", "public.yaml"],
  "document": { "version": "0.1.1", "semantic_model": [ … ] }
}
```

`verify`, `context`, `serve` and the deploy artifact read only this file:
none of them touch the directory or the network. It ships via the
digest-gated `sidecar()` helper in `deploy::artifact` and enters
`content_hash` (a semantic change rolls the workload). It stays **out of
the release meaning digest** (`release.rs` already excludes non-`CellYaml`
origins): an upstream edit must never move datamake's release gate. Known
cost, accepted: an Ossie edit that changes a supported column's meaning
draws no release warning; `synced_at` and `content_sha256` in the
document make it observable.

Staleness, ADR 0016 §5 verbatim: `cell_yaml_digest` mismatch ⇒ `serve`
refuses to start, `datamk context` warns on stderr and exits 0. Source
drift (dir content sha changed; git ref no longer at the recorded commit)
is checked by `context` only when the source is reachable; an unreachable
remote never blocks anything and is reported as "not re-checked since
`synced_at`".

## 5. Binding datasets to exports

At `config::load`, after discovery materializes the interface: a dataset
binds to an export when its `source`, normalized (surrounding `"` and
`` ` `` stripped per part, ASCII-lowercased), equals one of: the export's
discovered model name (ADR 0016, the SQLMesh FQN), the export's route key
(`name@major`), or the export's name. A dataset that binds to nothing is
kept, marked unbound, and excluded from route context — the same `osi/`
serves every cell cut from one repo. A dataset that binds to two exports
(two majors of one name) binds to both. Binding is never by dataset name.

## 6. Verify

For every bound dataset, in the existing per-export loop, all without
executing a query:

- **Fields.** An identity expression (the bare column name) must be a
  declared column of the export — hard error listing the declared columns.
  Any other `ANSI_SQL` expression: `DESCRIBE SELECT <expr> FROM <source>`
  must succeed — hard error quoting DuckDB's message and the sentence
  "nothing was executed". A field with no `ANSI_SQL` variant is recorded
  `verified: false, reason: dialect` and warned once per dataset.
- **`primary_key`.** Compared to the export's grain as a set. Differs ⇒
  hard error. Export has no grain ⇒ recorded `no_grain`, no error.
- **Relationships.** `from_columns`/`to_columns` must be fields or declared
  columns of their datasets when both sides are bound; same length; hard
  error otherwise. Unbound side ⇒ recorded `unbound`.
- **Metrics.** `ANSI_SQL` only: `DESCRIBE SELECT <expr> FROM (SELECT * FROM
  <src_a>) AS <dataset_a>, (SELECT * FROM <src_b>) AS <dataset_b> …` over
  every bound dataset of the model — identifier and type resolution, no
  join synthesized, no rows read. Any referenced dataset unbound ⇒
  `unverified: unbound`. Foreign dialect ⇒ `unverified: dialect`. DuckDB
  error ⇒ hard error.

Results are written to `.cell/semantic_check.json`, stamped with
`cell_yaml_digest`, `profile`, `checked_at` and the record's
`content_sha256`, and ride the context document as measurements.

## 7. Context document

Additive; `datamk_context` stays 4. Ossie content carries `from: ossie`
(a fourth `Origin`). The whole document is never served; four tiers:

| Door | Returns |
|---|---|
| `/context` | `semantic_models[]`: name, description, file, dataset/bound/metric counts. Always present, `[]` without a source. |
| `/context/<route>` | `exports[].semantic[]`: every dataset bound to the route, with fields, `ai_context`, and per-field verification; the model's relationships touching it (other side named by route or `null`); metrics whose datasets are all bound to this route, in full; other metrics naming the dataset as `{name, model}` pointers. |
| `/context?model=<name>` | one semantic model in full (`semantic_model` block), composable with `terms`. |
| `/context?terms=…` | Ossie dataset, field and metric names plus `ai_context.synonyms` join the lookup index beside `definitions:`; hits land in `semantic_matches[]` `{token, kind, model, dataset, field, description}`. Synonyms outside `[A-Za-z0-9_.-]`/64 chars are kept in the document and warned at `sync` as not addressable. Collisions across models return every hit, each naming its model. |

`datamk context --model <name>` mirrors `?model=`. The top-level
`semantic` block on the document carries `synced_at`, `content_sha256`,
`resolved` and the check's `checked_at` — measurements, outside every
digest. Lookup keys (model, dataset, field, metric names; synonyms) enter
`interface_digest`; prose and verification do not.

OpenAPI documents `model` on `/context` and the new schema pieces. MCP
adds the resource `datamk://<mount>/semantic/<model>` and lists it in the
not-found message.

## Refused

- Emitting Ossie from `cell.yaml` (round 1). May return as a re-emit of
  the ingested document; not in this ADR.
- Evaluating or serving a metric (`/metrics/<name>`): interpolating a
  file-supplied aggregate and a caller-supplied group-by is the injection
  surface `build_query` exists to exclude, and a served surface that is
  not an `Export` has no version, contract, route, or digest entry.
- Synthesizing joins from `relationships[]` for any purpose.
- A relaxed "datamake Ossie" dialect (e.g. optional `expression`): a file
  that is not valid Ossie cannot travel, which is the whole premise.
- Codegen of the structs from `ossie-schema.json`: it would emit
  permissive types wherever the spec is permissive without us choosing.
- A list of sources, `http(s)://`/`s3://` sources, a profile-bound
  location: meaning must not vary by environment.
- Anything datamake-specific living inside an Ossie document: a
  datamake-defined `ai_context` key (an estate-wide glossary term authored
  beside an Ossie model or dataset was considered and rejected), a
  `vendor_name: DATAMAKE` `custom_extensions[].data` payload datamake
  itself parses. `ai_context`'s object form still carries every unknown
  key through uninterpreted, exactly as the spec's own
  `additionalProperties: true` allows — datamake just never reads one back
  out. An expression-less term (no column, no expression to plan-check) is
  plain `ai_context.instructions` prose under this ADR, not a lookup key:
  it is not `?terms=`-addressable, the same line `definitions:` (ADR 0017)
  already draws for concepts Ossie cannot represent.

## Premises

1. `DESCRIBE`/`PREPARE` resolves identifiers and types without execution
   on every catalog tier.
2. The design partner's `osi/` exists and is the source of truth for
   relationships and field semantics (it does, as of 2026-09-10).
3. Ossie 0.2's open items (measures vs metrics, entity/grain, ontology)
   touch the read path only through the strict version pin.

## Falsifiers

1. Real glossaries turn out majority concept-only (no column, no
   expression) ⇒ `definitions:` is doing the work and Ossie is decoration.
2. Cross-dataset metrics need a synthesized join to verify ⇒ the honest
   answer is `unverified`, never a join planner.
3. Ossie 0.2 moves metrics to dataset-level measures ⇒ the version pin
   fails loudly on upgrade and the lookup keys change shape under authors.

## Amendment (2026-09-11): `?model=`/`?terms=` (no route) project `exports[]` too

§7's table says `/context?model=<name>` returns "one semantic model in
full, composable with `terms`" — true, but on the real 42-export/47-dataset
design-partner cell it arrived beside all 42 exports' full `schema`/
`check`/`semantic[]` bodies anyway: 467 KB, of which the requested model
was 38 KB. The door answered "the one thing you asked for" and "everything
else" in the same breath.

`GET /context?model=<name>` and `GET /context?terms=...`, absent a route,
now project `exports[]` through `ExportDoc::to_index` (ADR 0012 §4
amendment 2026-09-11, same date) — every export reduced to identity, its
claims, `columns[]` (names), and `semantic_datasets[]` (`model/dataset`
names) in place of `semantic[]`'s full dataset/field/relationship/metric
bodies. `semantic_model`/`semantic_matches` themselves are unaffected —
`?model=` still returns that one model in full; the projection only trims
the tag-along `exports[]`. `datamk context --model`/`--terms` (without
`--export`) match. `/context/<route>?terms=` keeps the route's full export
— see the ADR 0017 §2 amendment (same date) for the full reasoning, which
applies identically here since both doors share `?terms=`'s grammar.

MCP is unaffected structurally: `describe_export` already narrows to one
route (never carries the whole-cell `exports[]` to begin with), and the new
`datamk://<mount>/context/index` resource (ADR 0012 §4 amendment) is the
whole-cell projection's own door.
