# ADR 0014 — Multi-cell serving: `datamk.yaml` and mounted cells

- **Status:** Accepted — implemented 2026-08-11.
- **Date:** 2026-08-11
- **Deciders:** Datamake team
- **Author:** @scottypate

## Motivation

`datamk serve` serves exactly one cell per process. An operator with ten
cells runs ten processes: ten deployments, ten ports or ten DNS names, ten
things to start on a laptop before they can `curl` two of them.

The measured cost of a process is small — roughly 58 MB RSS idle, 0.09s cold
start, and the Kubernetes target sets no resource requests at all, so a
Server pod schedules as BestEffort and costs close to nothing. **This ADR is
therefore not justified by compute waste**, and a reader who evaluates it on
those grounds will conclude it isn't worth building.

It is justified by the decision it hands back to the operator. One cell per
process is a *policy* the tool imposes on every deployment, and it is the
wrong policy for a laptop, for a single VM, and for any estate where the
cells behind one port share a trust boundary anyway. A project file lets the
operator choose the packaging; it does not tell them which packaging is
right.

The corollary is that the single-cell path must stay exactly as it is. It
is the correct default and, on Kubernetes, still the recommended one.

## Decisions

### 1. `datamk.yaml` is composition only

```yaml
datamk: 1
profile: prod
cells:
  - datamk-examples/weather
  - path: dplat-datamake/flight-spend
    profile: local
    mount: flights
    no_data: true
```

The file names **which** cells this process serves and **where** they mount.
It cannot change what a cell means: identity, interface, and bindings stay
in the cell's own directory. Both structs are `deny_unknown_fields`, and an
unrecognized `datamk:` version is a hard error — the version key is also the
discriminator `-f` dispatches on (`datamk:` = project, `cell:` = cell), so it
is required rather than defaulted.

Paths resolve relative to the project file and are **canonicalized at load**:
the poller re-opens each cell from its path on every swap, and a relative
path resolved against the process cwd would work at startup and then fail
fifteen seconds later with nothing but a `tracing::warn!`, silently freezing
that cell on its opening execution.

`datamk.yaml` is deliberately **not** a mesh manifest and not a registry.
Every entry is a local path the operator already owns, read off the
filesystem at startup. Nothing is fetched, nothing is indexed, and no cell
learns that another exists.

### 2. Per-cell `profile:` is allowed, and it is the sharpest edge here

A cell may pin its own profile. This was contested: a profile carries
credentials, a catalog binding, and the `principals:` file, so a cell pinned
to `profile: local` inside a production deployment serves a local catalog
under a local principals file — the whole default-deny policy swapped out,
with the pod still green on `/`.

It is in because the operator asked for the freedom and the file is
reviewable. The mitigation is **visibility, not prevention**: the startup
banner prints mount, cell, profile, and data-mounted state for every cell, so
a wrong profile is visible in the first ten lines of the log rather than
inferred from behavior. `serve -p` overrides the project default *and* every
per-cell pin — a flag that silently no-ops on some cells is worse than one
that overrides everything, and `deploy --target` already set the CLI-beats-
file precedent.

**If this produces a production incident, the fix is to delete the field**,
not to add a guard rail around it.

### 3. Request affordances are relative to the document's own URL

`declared.include_request`, `declared.exports[].query.sample_request`, and
`observed.exports[].example_request` now read `context?include=docs` and
`orders_daily@2?limit=10` — no leading slash. `DATAMK_CONTEXT_VERSION` bumps
to **3**.

This is the load-bearing change and it is not cosmetic. `sample_request`
lives inside `Declared`, which `interface_digest` serializes whole, so a
root-absolute affordance forced a choice with no safe branch once a cell can
be mounted at a base path:

- Leave it `/orders_daily@2?limit=10` and it points at the *process* root.
  In a project that is another cell's mount or a 404, handed to an agent as
  its first working call.
- Prefix it with the mount and the same `cell.yaml` yields two different
  digests depending on where it is served. That puts deployment inside the
  contract: `mesh`'s `context_digest` stops being a verifiable cache, and the
  two-doors test breaks outright because `datamk context` has no mount to
  prefix with.

A relative reference resolves per RFC 3986 §5 against the document's own URL
and is correct in both modes with one string. The digest never sees the base
path — which is the invariant, not the mechanism: **`base_path` is
deployment and must never reach `interface_digest`.**

### 4. Each mounted cell gets its own concurrency cap; the project root is exempt

A tower layer instance owns one semaphore, so a single shared throttle across
mounted cells would let one cell's saturation shed every other cell's
requests — including their liveness route, which the Kubernetes target wires
to `GET /` for *both* readiness and liveness. One slow query would become a
pod restart taking every cell down, each re-downloading its artifact on the
way back up. `--max-concurrency` therefore keeps its single-cell meaning and
applies per mounted cell. The project root sits outside every throttle layer
so no cell can shed the process's liveness.

### 5. The root lists mounts the caller can actually reach — and nothing else

`GET /` returns `{"status": "ok", "cells": [...]}`, filtered through each
cell's own `authorize()`. An anonymous caller against an all-private project
gets `[]`; a non-shareable cell is invisible to everyone, exactly as it is
today. `status` stays unconditional so a liveness probe needs no credential.

It carries **names only** — no exports, no descriptions, no digests. ADR 0012
§6 refuses "a data-plane route that enumerates other cells" as a registry
endpoint and pre-auth crawl bait, and the moment this route returns a per-cell
summary it *is* a served mesh manifest. That document is `datamk mesh emit`'s
job: a static file an operator hosts, never this socket.

Three invariants keep process co-tenancy on the right side of the
no-control-plane thesis, and they are decisions, not prose:

1. **The root makes no claims about cells.** Names, or nothing.
2. **No aggregation at project scope, ever.** No root `/openapi.json` unioning
   specs, no root `/context`. Each cell's document stays the sole owner of its
   own strings.
3. **No cell-to-cell reference over this process's HTTP surface.** Composition
   stays through the governed catalog. Two co-mounted cells talking over
   localhost HTTP is the server-side aggregator smuggled into the data plane.

### 6. Startup is all-or-nothing

Every listed cell opens or the server does not start. The precedent is
`load_principals`, which fails loud precisely so a swallowed error cannot
"silently start an all-deny server (or, worse, look healthy via `/health`
while denying every request)." A partially-mounted server passes its probe
while serving a 404 no caller can distinguish from a typo. The existing
asymmetry is preserved: **startup strict, runtime tolerant** — the poller
keeps last-good on failure because there is a known-good catalog to keep; at
startup there isn't. An operator who wants a cell out removes it from the
file.

Every authoring error (bad version, empty list, missing definition, illegal
or duplicated mount) surfaces in one pass before any cell opens, so an author
fixes them together and only environment errors arrive one at a time.

### 7. Resource budgets are divided, not repeated

`DATAMK_MEMORY_LIMIT` is applied **per DuckDB connection**. Applying the raw
value to N connections authorizes N times the operator's budget, and the pod
OOM-kills taking every mounted cell with it — the precise failure the knob
exists to prevent. `serve` divides it and passes an explicit `engine::Budget`;
an unparseable value is a hard startup error rather than a warning. DuckDB's
`threads` default is the host core count per connection, right for one
connection and N times too many for N, so project mode sets it. Cells open
sequentially (every `setup` runs `INSTALL ducklake` against the shared
extension directory), and pollers are staggered so N threads don't wake
together and issue N simultaneous artifact downloads forever.

### 8. `deploy` stays per-cell

The Kubernetes target renders one ConfigMap, Deployment, Service, and CronJob
per cell, selected by `app_label(cell)`, with `checksum/config` carrying that
cell's content hash. A multi-cell Deployment needs N ConfigMap mounts, a
merged principals mount, and — decisively — a merged checksum, so publishing
cell A's interface would roll the pod and drop cell B's warm catalogs. That
trades N pods for coupled rollouts and a shared failure domain. It is a real
tradeoff with a real answer and it is not this ADR's answer.

Multi-cell `serve` is the local, single-VM, and behind-your-own-proxy story.
`datamk deploy -f datamk.yaml` is not supported.

## Consequences

- Single-cell serving is byte-identical: flat routes, no `servers` block in
  the OpenAPI document, same throttle semantics, same headers. Nesting it
  would break every deployed URL, every probe path, and every `mesh emit` url.
- A cell's interface digest is the same mounted or unmounted. Asserted
  directly by test, because it is the property decision 3 exists to buy.
- `mesh` needs no change: `context_endpoint` appends `/context` to a base, so
  a manifest entry is `{"name": "weather", "url": "https://host/weather"}`.
- A `mount:` override desynchronizes `mesh emit --store --url-template
  "https://host/{name}"`, which derives names from store prefixes. Documented
  cost of the override, not a reason to refuse it.
- Data routes now send `Cache-Control: private`. One origin serving many cells
  with different policies makes a shared cache keyed on URI alone a
  cross-tenant leak; `/context` and `/openapi.json` already said `private`.
- Every poller line carries `cell`, without which N interleaved pollers on one
  stderr are unattributable.
- `serve`'s `-f` and `-p` became optional, so "the user passed it" stays
  distinguishable from "clap filled it in".

## What would reverse this

- **A cell pinned to the wrong profile causes a production incident.** Delete
  per-cell `profile:` (decision 2); profile becomes purely a CLI argument.
- **Anyone asks to co-mount cells with materially different sensitivity.**
  The auth boundary argument wins and this stays a dev/single-VM feature,
  documented as such.
- **A root listing shows up in a crawl or an agent starts routing off it.**
  Drop the `cells` array to `{"status": "ok"}` and let `mesh emit` own
  discovery entirely (decision 5).
