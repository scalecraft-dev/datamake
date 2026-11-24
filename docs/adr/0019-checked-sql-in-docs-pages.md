# ADR 0019: Checked SQL in docs pages (`check:` and a dry-run verify step)

- **Status:** Accepted, 2026-11-24.
- **Date:** 2026-11-23
- **Deciders:** Datamake team
- **Author:** @tonytusharjr
- **Depends on:** ADR 0007 §4 (the dry-run preflight), ADR 0013 (docs pages),
  ADR 0017 (`definitions:`, the page this is mostly for).

## Motivation

A design partner serves question playbooks through a discovered cell: one
`definitions:` entry per kind of question whose rule the user picks ("churn,
under any window or grain"), each with a long-form page of copy-and-edit SQL
recipes over the cell's own exports. ADR 0017 made that work well: an agent
asks `?terms=churn&include=docs` and gets one recipe page, not the glossary.

What nothing checks is the SQL in those pages. `verify` holds an export's
columns, grain and types to the warehouse; a recipe naming a column the
upstream model just renamed passes every check and fails only when an agent
runs it, in front of a user, with a confident-sounding error. The page is
versioned with the schema it explains (ADR 0013's whole point), but nothing
connects the two at verify time.

The building block exists. ADR 0007 §4 already dry-runs an author's query
through the connector (`ResolvedConnection::query_dry_run_sql`, BigQuery's
`dry_run := true`: free, unbilled, no scan) and already classifies a failure
as "the author's query is wrong" versus transport
(`is_dry_run_query_error`). This ADR points that machinery at docs pages.

## Decision

### 1. Authoring surface: opt in per page with `check:`

`check:` names a connection from the profile. It is accepted wherever a
`docs:` page is declared (the cell, an export, a definition) and only
there:

```yaml
definitions:
  - term: net_revenue
    description: Invoiced revenue less credit memos.
    docs: docs/terms/net_revenue.md
    check: warehouse          # dry-run this page's SQL blocks against `warehouse`
```

Opt-in, not default: most pages hold prose and illustrative fragments that
were never meant to run. A page that opts in is promising its SQL runs.

### 2. What is checked

Every fenced block whose info string is exactly `sql` in an opted-in page,
in document order. A block that is deliberately not runnable (a fragment, a
"don't do this") opts out with the info string `sql no-check`. No other
parsing: no templating, no parameter substitution. A recipe the author wants
checked must run as written with its default parameters, which is also what
an agent copying it needs.

### 3. When it runs: `verify`, never `serve`

`datamk verify` gains the step after its existing checks. For each opted-in
page, each block goes through `query_dry_run_sql` on the named connection:

- **Query error** (`is_dry_run_query_error`): verify fails, naming the page,
  the block's index and line, and the warehouse's message. This is the
  author's SQL being wrong, the case the feature exists for.
- **Transport error:** warns and continues, as ADR 0007 §4's preflight does.
- **Connector with no free dry run** (Snowflake, Postgres, DuckDB today):
  the page is skipped with one note naming the connector. An expected
  capability gap, not a failure, same rule as §4.

`serve` never runs checks: a docs page must not be able to stop a server
from starting, and handlers never touch the warehouse for docs (ADR 0013).

### 4. Where the result lives

**Deferred to a follow-up.** This slice ships the check itself (sections 1 to
3) and nothing else; a result recorded in `observed` lands separately.

The decision when it does: a check result is a machine fact, so it goes
beside the page's existing fingerprint in `observed`, carried through
`published.json` like `docs[].{sha256, bytes}`:
`{checked_at, connection, blocks, failed}`. Never in `declared` and never in
the interface digest: a recipe edit is not an interface change (ADR 0013 §5's
reasoning, unchanged). An agent reading a page can then see whether its SQL
was last checked and when.

## Consequences

- Recipe rot surfaces in CI, where `verify` already runs, instead of in a
  user's session.
- Dry runs cost nothing on BigQuery; a page with ten blocks is ten API calls
  during verify.
- Pages that opt in must keep their default parameters runnable. That is a
  real constraint on authors and the intended one.

## Resolved questions

1. **Deployed cells that verify on start.** `verify` gains
   `--no-doc-checks`, which skips the whole step, connection-name validation
   included, so prose can never block a rollout. It stays on by default and CI
   runs the full check.
2. **Info string.** `sql no-check` is the per-block opt-out, and the page is
   still the opt-in: an untouched page costs nothing, and an author never has
   to remember to opt each runnable block in.
3. **Scope.** The cell page is included. `check:` is accepted anywhere a
   `docs:` page is declared (cell, export, definition), and it is an error to
   set it without a page.

## Alternatives considered

- **Check in the consumer's CI instead.** Works, but every consumer
  re-implements extraction, connection handling and error classification the
  engine already owns, and the result never reaches `observed`, so an agent
  cannot see it.
- **Check every `sql` block by default.** Turns every illustrative fragment
  into a failure; authors would learn to avoid fenced SQL, which is worse.
- **A `recipes:` key separate from docs pages.** A second home for the same
  prose, the drift ADR 0012 §4 and ADR 0013 refused.
