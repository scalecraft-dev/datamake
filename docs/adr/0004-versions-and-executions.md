# ADR 0004 — Versions, executions, and the published-catalog serving model

- **Status:** Proposed (revised after team review 2026-07-03)
- **Date:** 2026-07-03
- **Deciders:** Datamake team
- **Author:** @scottypate
- **Depends on:** ADR 0001 (deploy contract), ADR 0002 (Kubernetes target)
- **Supersedes:** ADR 0001 §7 (metadata-DB catalog requirement) and §9 (live-
  catalog freshness); the corresponding pre-flight and serve behavior in ADR 0002

## Scope

This ADR fixes the vocabulary the deploy model is built on — **version** vs
**execution** — and replaces the shared live catalog (Postgres) with
**published, immutable catalog artifacts** in the object store. It covers the
artifact layout and commit protocol, the object-store client this requires,
the Builder's publish step, the Server's fetch-and-swap, `release` under the
new model, rollback semantics, retention, observability, the pre-flight and
schema changes, and cross-cell composition. ADR 0003 (warehouse sources) is
unaffected.

## Context

### The vocabulary error in ADR 0001

ADR 0001 modeled a deployed cell as two workloads sharing one live catalog:
the Builder commits snapshots through it and the Server reads through it, so
what the Server serves changes underneath it as the Builder runs. To make
that concurrency safe, §7 *requires* a metadata-DB catalog — in practice a
Postgres instance — for every deployed cell.

That requirement is the tell that something upstream is wrong. Datamake's
whole identity is the embedded, zero-services engine, and the deploy story
made "bring a Postgres" the entry fee for a cell's **own private state**. The
root cause is a conflated vocabulary:

- A **version** is the cell definition — `cell.yaml`, the transform SQL, the
  interface. It changes through a git process (PR, review, merge) and lands
  via a deployment. Rare, human-paced, governed.
- An **execution** is the pipeline running on its schedule *under* a version.
  Same code, same contract, same tables — fresh data flowing through.
  Frequent, mechanical, not a release. Nobody redeploys an API because it
  handled new requests; an hourly run is not a new version of the cell.

The live-catalog design entangles these: an execution (a data refresh)
mutates the very object the serving plane holds open, which forces the
shared-database machinery, which drags in Postgres. Meanwhile the product
already takes the opposite position everywhere else — DuckLake snapshots are
immutable, `release` pins them, supported routes refuse to drift.

### Why the file catalog couldn't just be shared

The obvious repairs don't work, and it's worth recording why:

- **A `.ducklake` file on a shared volume.** DuckDB's cross-process rule is
  one read-write attach *or* many read-only attaches — never both — so any
  live reader blocks the Builder's commit indefinitely. Kubernetes access
  modes make it worse: `ReadWriteMany`/`ReadOnlyMany` exist only on network
  filesystems, exactly where the file locks DuckDB relies on are unreliable;
  common block storage can't multi-attach at all. And `ReadOnlyMany` still
  excludes a writer while readers are attached — the same deadlock, enforced
  by the infrastructure.
- **A per-cell managed Postgres.** Deploy could provision one, but running a
  foreign stateful service per cell to hold a few kilobytes of a cell's
  *private* state is the wrong weight class and the wrong identity for an
  embedded engine.

The resolution is not to share the catalog at all.

## Decision

Adopt the version/execution vocabulary, and make every execution **publish an
immutable catalog artifact** to the object store. Nothing shares a mutable
file; every reader reads a private local copy. Postgres exits the deployed
architecture.

### 1. Versions deploy; executions publish

- A **version change** is a git change deployed with `datamk deploy`: new
  pods, catalog structure initialized, first execution run (the init Job,
  ADR 0002 §1, unchanged in role). The pod template is stamped with the
  version's content hash (already true via the content-addressed ConfigMap),
  so a version is literally a distinct ReplicaSet — rollback of a *version*
  is a rollout undo.
- An **execution** is a scheduled Builder run under the deployed version. It
  commits a snapshot and publishes the result (§4). Serving pods live across
  executions — they are the same version — and advance their view of the
  data per execution (§6). Pods restart only on version deploys.

### 2. Artifact layout

Everything a cell publishes lives under its storage prefix, next to the
Parquet it already writes:

```
s3://<bucket>/cells/<cell>/
  data/…                          # Parquet written by executions (DuckLake DATA_PATH)
  catalog/
    executions/00000047.ducklake  # immutable catalog artifact for execution 47
    LATEST                        # pointer: the currently served execution number
```

- Execution numbers are zero-padded (8 digits) so lexicographic object
  listing is numeric ordering. `LATEST`'s content is **exactly the padded
  number as UTF-8, no newline** (`00000047`) — this format is normative,
  because §9's escape hatch has operators writing it by hand.
- An artifact is a complete DuckLake catalog (a DuckDB file) copied after the
  execution's commit, while detached — quiescent because publish only happens
  after a clean detach, and the overlap guard (§5) prevents a second writer.
- **Append-and-republish:** an execution starts from the artifact `LATEST`
  points at and commits into it, so each artifact carries the snapshot
  history of its lineage. Immutability lives at the artifact level (no
  published object is ever rewritten); history lives inside the artifact.
  Note the artifact sequence is a *lineage*, not necessarily a straight
  line: after a rollback (§9), the next execution's parent is the
  rolled-back-to artifact, and the superseded artifact remains in the store
  as an immutable dead branch. `LATEST`'s trajectory over time is the served
  history.

### 3. The object-store client (a new, explicit subsystem)

The engine today touches object storage only *through DuckDB* (httpfs + a
DuckDB secret) — a query-engine capability. This ADR additionally needs
plain object operations DuckDB's SQL surface does not expose: GET a small
pointer, LIST a prefix, PUT a file, **conditional PUT** (create-if-absent).

- datamk gains a native object-store client behind a small internal trait
  (`src/store.rs`), implemented with the `object_store` crate — chosen
  because its `PutMode::Create` conditional write is supported across S3,
  GCS, Azure, and local filesystems, matching the ADR's provider-agnostic
  framing.
- **Credential parity is a requirement, not an aspiration:** the client
  resolves credentials and connection shape from the *same* `s3:` profile
  block (`S3Binding`) that configures DuckDB's secret — endpoint,
  `url_style`, `use_ssl`, region, explicit keys or the ambient chain. One
  profile block drives both clients; a configuration that works for the
  query engine must work for the publisher, or the profile contract is a
  lie.
- **Conditional-PUT support is a capability probe, run where connectivity
  actually exists.** The entire overlap-safety story (§5) rests on this one
  primitive, and S3-compatible stores vary (AWS shipped it in 2024; MinIO
  support is version-dependent). The probe (create-if-absent on a scratch
  key under the cell's prefix, twice — the second must fail) runs in two
  places: **authoritatively at the start of every published-mode `run`** —
  in the process that publishes, with the pods' connectivity, so via the
  init Job a failing store fails the deploy with the build pod's logs — and
  **best-effort on the deploy host**, where a store that answers and proves
  non-enforcement refuses the deploy immediately, but *unreachability
  defers* to the in-pod probe (the deploy host may legitimately be unable
  to reach an in-cluster or private-endpoint store that pods can — found by
  the kind e2e harness, whose MinIO is cluster-DNS only). Fail loud before
  building, not last-writer-wins at 3am.

### 4. The Builder's execution loop (commit, then publish)

1. Read `LATEST`; download the artifact it names to local scratch. If
   `LATEST` is absent (**bootstrap**: the very first execution, run by the
   init Job), initialize a fresh catalog locally instead — this is the
   distinct first-execution path, and it is the only time one is created.
2. Attach read-write locally, bind sources, run transforms in one
   transaction, commit the snapshot — the existing `engine::run`, operating
   on a private local file (DuckDB's designed-for case).
3. **Verify gates publish.** `verify::check` runs against the committed
   result; on failure the execution aborts *before* any upload. A failed
   contract check never enters published history.
4. Detach cleanly. Compute the next execution number as
   **`max(LIST catalog/executions/) + 1`** — *not* `LATEST + 1*. This is
   what keeps rollback (§9) from wedging the Builder: after `LATEST` is
   repointed backwards, the next execution still allocates a fresh number
   above every artifact that exists. LIST is only used for numbering; a
   stale listing at worst produces a collision that the conditional PUT
   rejects loudly, and the run retries with a fresh listing.
5. Upload the file as `executions/<N>.ducklake` with a **conditional PUT
   (create-if-absent)** — the enforced guard against any concurrent writer.
   Then overwrite `LATEST` with `N`. Object-store PUTs are atomic per
   object and read-after-write consistent, and the pointer is written
   *last*, so a reader can never observe a pointer to a missing artifact. A
   crash between the two PUTs leaves an orphaned artifact and an unchanged
   pointer — harmless; the next execution numbers past it.

### 5. Single writer: by convention, enforced by the store

The Builder CronJob runs with `concurrencyPolicy: Forbid` and a
`startingDeadlineSeconds` (so long builds can't accumulate missed starts
until the controller stops scheduling). But Forbid is **convention** — it
gates the CronJob controller's own scheduling, not a manual
`kubectl create job --from=cronjob/…` or a controller restart. The
*enforced* guard is §4's conditional PUT: a rogue second writer loses the
artifact-key race and fails loudly. It cannot silently clobber an
execution. (This is why §3's capability probe is a hard gate: without
conditional PUT there is no enforcement, only convention.)

### 6. The Server: fetch, attach locally, swap

- On startup, `serve` reads `LATEST`, downloads the artifact to local
  scratch (emptyDir), attaches it read-only with `OVERRIDE_DATA_PATH true`
  (the artifact records the DATA_PATH of whatever host built it; the
  profile's `storage` is authoritative — the same trust-what-you-were-handed
  rule `bind_source` already applies to cell sources), and serves. In the
  deployed flow `LATEST` always exists (the init Job published execution 1
  before the Server was applied — ADR 0002's ordering, preserved); if it is
  nonetheless absent, serve retries with backoff and logs loudly rather
  than serving an empty catalog.
- On a poll interval it re-reads `LATEST` — one small GET. On a new N:
  download, attach on a fresh connection, and swap the serving connection.
  The swap is a handle replacement (`ArcSwap`-shaped: requests take a cheap
  clone of the current handle; the poller replaces it); in-flight requests
  finish on the generation they started with, and no request ever spans two
  catalogs. Honestly stated: an in-flight query pins its generation (and
  its local file) until it completes, so resident generations are bounded
  by in-flight work, not by "two." The poller **does not advance** past one
  pending generation — if the previous swap hasn't fully drained, it waits —
  which bounds memory and disk at two generations, and each superseded
  artifact's local file is deleted after drain (emptyDir counts against
  node ephemeral storage; unbounded retention there is a pod-eviction
  vector, not an implementation detail).
- **Configuration home:** the poll interval is ops tuning, so it lives in
  the tracked deploy overlay (`serve.poll_interval`, seconds, default 15),
  rendered onto the container as a `--poll-interval` flag on `datamk
  serve` — per ADR 0001 §6's trust split it must not live in the
  secret-bearing profile.
- **Freshness contract:** bounded staleness (the poll interval) for
  experimental "latest" routes; supported routes are pinned by `release`
  and indifferent to newer executions. Effective freshness is bounded by
  execution cadence anyway; the docs promise the mechanism and the default,
  not a hard real-time number.
- **Observability (bounded staleness must be visible or it's a lie):** the
  pre-authorize liveness route reports the currently served execution
  number (a low-sensitivity monotonic counter, enough for a smoke test to
  confirm a swap). Full freshness detail — served N, `LATEST` seen, last
  successful poll time — is exposed *behind auth* on `/interface`. A wedged
  poller (object store unreachable) keeps serving last-good data, and this
  signal is what stops that from being invisible. Note the pre-authorize
  route is `/` in the code today, not `/health` as ADR 0001's prose says —
  this ADR treats `/` as the fact and 0001's prose as the erratum.
- Any number of replicas: each pod holds its own private local file. The
  `replicas > 1 ⇒ Postgres` pre-flight rule is deleted. Adjacent replicas
  may briefly serve different executions within one poll interval; that is
  inherent to the model and documented.

### 7. `release` under the published model

`release` today attaches the live catalog to resolve the current snapshot
id — the same pattern serve had, and it breaks identically. Under this ADR,
`release` reads `LATEST`, downloads that artifact, attaches read-only, and
pins route → snapshot id from it, exactly like serve's startup path. It is
a first-class work item, not fallout.

### 8. Pinning semantics: `version:` still means *snapshot*

`Source::Cell.version` and the release pin both refer to **DuckLake
snapshot ids** today, and this ADR does not change that contract. Because
artifacts append-and-republish (§2), the artifact `LATEST` names contains
every snapshot in its lineage — so a snapshot pin is resolvable against the
current artifact without any new "execution pin" concept. Execution numbers
are an *operational* coordinate (what `LATEST` points at, what rollback
targets); snapshot ids are the *contract* coordinate (what `version:` and
`release` name). The two numbering spaces never mix in user-facing fields.

### 9. Execution rollback: a command with a pin guard

A bad *version* rolls back via the deployment (§1). A bad *execution* rolls
back by repointing `LATEST` — but this is **not** advertised as "one small
PUT," because a bare PUT has three sharp edges: the pointer byte format is
easy to get wrong, the rollback can strand a release pin, and the operator
must understand that the next scheduled execution *continues from the
rolled-back state* (it builds on N−1's lineage and publishes a fresh
number — see §4 step 4; the bad artifact stays as a dead branch). So:

- **`datamk rollback -f cell.yaml -p <profile> [--execution N]`** ships
  with this ADR (default: the execution before the one `LATEST` names). It
  validates that artifact N exists (listing the available range on error),
  **refuses any rollback to an artifact that does not contain every
  currently pinned snapshot** — this is the guard that keeps a supported
  route from 500-ing on `AT (VERSION => <pinned>)` against an artifact
  from before the pin — and writes the pointer in the normative format.
  Its help text disambiguates the two rollbacks: this one moves *data*
  (executions); version/code rollback is the orchestrator's rollout undo.
- The manual PUT remains documented as an escape hatch, with the format
  from §2 and the same pin warning. If the operator wants the world frozen
  at the rolled-back state, they suspend the CronJob — the command says so.

### 10. Retention and compaction: a v1 companion, not follow-up

Append-and-republish makes execution N cost `download(size_N) + build +
upload(size_N+1)` with size growing in snapshot count — O(N) per run,
O(N²) cumulative. For the hourly cell this model is optimized for, catalog
I/O becomes the dominant and growing cost, so compaction **ships with the
model**, not after it:

- The Builder, as part of an execution, expires snapshots older than a
  retention window — **never expiring any pinned snapshot**. Pins are read
  from the release manifest (`.cell/published.json`), which travels with
  the cell content; the compactor's rule is `expire only snapshots older
  than the window AND older than the oldest pin`.
- Superseded execution artifacts (including dead branches from rollbacks)
  are garbage-collected after a grace period, except any artifact `LATEST`
  currently names or named within the grace window.
- Until this exists, snapshot history inside artifacts is a *mechanism*
  (it keeps pins and future incremental builds working), **not a marketed
  feature** — the README does not promise "time travel" on deployed cells
  in v1.

### 11. Profile and pre-flight changes (explicit schema migrations)

- **`Bindings.catalog` becomes `Option<String>`.** This is a breaking
  schema change and is stated as such. Mode selection is by presence:
  - `catalog:` present → **direct-attach mode**, exactly today's behavior
    (local file, or a self-managed `sqlite:`/`postgres:` DSN — both remain
    supported for local dev and self-managed setups).
  - `catalog:` absent → **published-artifact mode**; the catalog location
    derives from `storage` (§2 layout). Absent catalog requires remote
    `storage`; absent + local storage is an error, not a guess.
- **Deployed profiles must omit `catalog:` entirely.** Pre-flight rejects
  *any* value — not just DSNs; an absolute file path is equally unreachable
  from a pod — with an actionable message in the house style:

  ```
  deploy: profiles/prod.yaml sets `catalog:` (postgres://…), but a deployed
  cell derives its catalog from `storage` and publishes an immutable catalog
  artifact per execution — it has no separate catalog DSN. Remove the
  `catalog:` line; `storage` is a deployed cell's only external dependency.
  See ADR 0004.
  ```
- **`CellLocation.catalog` becomes `Option<String>`** with the same
  presence rule: present → direct attach of the upstream (local dev,
  self-managed); absent → published mode against the upstream's storage
  prefix. Existing profiles with `cells:` maps keep working; deployed ones
  drop the catalog line. The scaffold, fixtures, and tests are updated.
- Pre-flight, replacing ADR 0001 §7's catalog rules: `storage` must be an
  object store (unchanged — now the *only* external dependency); the
  conditional-PUT capability probe (§3) must pass; the `replicas > 1`
  catalog restriction is removed.

### 12. Cross-cell composition reads published artifacts

`Source::Cell` today attaches the upstream cell's *live* catalog — reaching
into another cell's private internals. Under this ADR (published mode, per
§11's presence rule) it instead: resolves the upstream's `catalog/LATEST`
from the storage prefix, downloads that artifact to local scratch, attaches
it read-only with `OVERRIDE_DATA_PATH true`, and reads the table —
fetch-local-then-attach, the same mechanism as serve, *not* an attach over
`s3://` (attaching DuckLake's catalog store remotely is not the known-good
path; the Builder's own loop concedes this by downloading first). The
`version:` pin keeps snapshot semantics per §8. Downstream cells compose on
released, versioned state — the same contract discipline the interface
enforces, applied to lake-level composition.

This layout is also the documented **ad-hoc consumer surface**: an analyst
with bucket credentials attaches `executions/<N>.ducklake` read-only from
their own DuckDB and gets full SQL over the cell's tables at a known
execution — the capability people used the shared Postgres for, without a
live catalog to contend on. It gets a README section, or users will
perceive a capability loss that didn't happen.

## Consequences

- **A deployed cell's only external dependency is a bucket.** No Postgres,
  no metadata DB, nothing to provision for the cell's private state. The
  deploy examples collapse to: have a bucket, `datamk deploy`.
- ADR 0001 §9's live-freshness machinery is deleted, not reimplemented:
  serve's freshness contract becomes "bounded staleness (poll interval) for
  latest; frozen for supported," with the staleness signal exposed per §6.
- **The work, honestly (roughly in dependency order):**
  1. `src/store.rs` — object-store client behind a trait; credential parity
     with `S3Binding`; conditional-PUT probe. *The one piece with no
     existing seam; settle the interface first.*
  2. Config schema: `Bindings.catalog` and `CellLocation.catalog` →
     `Option<String>`; mode-by-presence resolution; test rewrites (several
     existing tests assert the inverted pre-flight and required fields).
  3. Engine: fork `setup`/`open` into direct-attach vs published modes;
     `fetch_latest` / `download_artifact` / `publish_execution` helpers;
     bootstrap path; rewrite `bind_source`'s Cell arm onto the same
     helpers; `OVERRIDE_DATA_PATH` on published-mode attaches.
  4. Serve: startup fetch; poller (`tokio` task, blocking work via
     `spawn_blocking`); handle-swap with the two-generation bound and
     local-file reclaim; execution number on `/`; freshness detail on
     `/interface`; `--poll-interval`.
  5. `release`: fetch-and-attach instead of live open (§7).
  6. `datamk rollback` (§9) and `datamk status` (print published range,
     `LATEST`, pointer age — the read side that makes operations legible;
     it needs only bucket credentials, no cluster access).
  7. Pre-flight rewrites (deploy-generic + Kubernetes), overlay
     `serve.poll_interval`, scratch-volume render, `startingDeadlineSeconds`.
  8. Compaction (§10) — in v1, pin-aware.
  9. e2e harness: drop Postgres, keep MinIO, and **empirically verify
     conditional PUT against the pinned MinIO version** — the sharpest
     assumption in this ADR, and it must be proven where the tests run.
     Swap-under-load behavior gets a load test, not an assertion.
- Executions can be as frequent as the pipeline can run — cadence is
  bounded by build time plus catalog I/O (kept flat by §10's compaction),
  not by deployment mechanics. Versions stay rare because git gates them.
- `datamk init` scaffolding and example profiles drop the catalog DSN from
  deployable profiles (`storage` alone makes a profile deployable), fix the
  now-false `replicas > 1 requires postgres` guidance, and stop teaching
  the `file → sqlite: → postgres:` escalation ladder — there is no ladder;
  there is local dev and there is published mode.
- **Sequencing (product):** this lands before deploy goes wide — shipping
  the Postgres model and then walking it back is churn we can skip — and
  ahead of the ADR 0003 implementation push.

## Alternatives considered

- **Keep the shared live catalog (Postgres) — the ADR 0001 status quo.**
  Rejected: it prices a cell's private state at one managed database, and it
  lets served data change with no version event, no cutover, and no
  rollback point — at odds with the product's own snapshot/pinning
  discipline.
- **Deploy provisions a per-cell Postgres.** Rejected: removes the
  operator burden but keeps the wrong architecture — a foreign stateful
  runtime as the custodian of an embedded engine's private state, plus
  backup/upgrade/lifecycle obligations datamk shouldn't own.
- **Share the `.ducklake` file on a RWX/ROX volume.** Rejected: DuckDB's
  single-writer-or-many-readers rule makes readers block the writer; ROX
  block storage excludes writers at the infrastructure level; and network
  filesystems undermine the file locks that make any of it safe (see
  Context).
- **A deployment per refresh (immutable pods carrying the data).**
  Rejected: it conflates execution with version. Refresh cadence becomes
  deployment cadence, deploy history becomes data-refresh spam, and the
  governance value of version events is diluted to zero. Versions are
  git-gated; executions must not roll pods.
- **Single-pod cells (Builder folded into the Server process).** Rejected
  as the general model: it caps serving at one replica and still needs a
  publish story for cross-cell reads. The published-artifact model
  subsumes it — a one-replica cell is just the N=1 case.
- **Fresh catalog per execution (no history carry-over).** Rejected:
  simpler artifacts, but time travel, upstream pins, and incremental builds
  all reset every execution; history has to live *somewhere*, and inside
  the artifact (append-and-republish) is where DuckLake already knows how
  to manage it.
- **Deriving the next execution number from `LATEST + 1`.** Rejected
  (found in review): combined with conditional-PUT immutability it wedges
  the Builder permanently after any rollback — the next run recomputes a
  number that already exists and collides forever. List-derived numbering
  costs one LIST per execution and tolerates rollback by construction.
- **An execution-number pin in `cell.yaml`** (`version:` meaning artifact
  N). Rejected: it would silently change the meaning of a published
  contract field that today names DuckLake snapshots, and snapshot pins
  already resolve against any artifact in the lineage (§8). Executions
  stay operational; snapshots stay contractual.
- **Unconditional PUTs guarded only by `concurrencyPolicy: Forbid`.**
  Rejected: Forbid is scheduling convention, bypassable by manual jobs and
  controller restarts; without conditional PUT the failure mode is silent
  last-writer-wins corruption in exactly the scenarios (operator
  intervention) where it's most likely. Stores that can't prove
  conditional PUT are refused at deploy pre-flight.
