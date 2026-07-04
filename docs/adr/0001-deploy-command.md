# ADR 0001 — `datamk deploy`: deploy cells to an orchestrator

- **Status:** Proposed
- **Date:** 2026-06-29
- **Deciders:** Datamake team
- **Author:** @scottypate
- **Superseded in part by ADR 0004:** §7's metadata-DB catalog requirement and
  §9's live-catalog freshness model are replaced by published, immutable
  catalog artifacts (versions deploy; executions publish).

## Scope

This ADR defines the **target-agnostic deploy contract**: the `deploy` command,
the `release` rename, the `DeployTarget` trait seam, the tracked deploy overlay,
versioned base images, and the invariants every backend must uphold. The
Kubernetes-specific realization of all this — manifest rendering, the apply
mechanism, ConfigMap/Secret wiring — lives in **ADR 0002**. New orchestrators
(Airflow, Dagster, ECS) get their own ADRs against this same contract.

## Context

Datamake has two production-shaped workloads but no way to run them somewhere
durable:

- the **Builder** (`datamk run`) rebuilds snapshots, and
- the **Server** (`datamk serve`) exposes the cell's interface as REST + OpenAPI.

Both work today as local processes. A cell is only useful in production when
both run somewhere managed — on a schedule and continuously, respectively. We
have no command for that.

### What `release` does

Promotion and pinning are two distinct facts about a `supported` export, and stay
separate:

- **Promotion is a manual `cell.yaml` edit.** Declaring an export
  `contract: supported` is a hand edit to `cell.yaml` (scaffolded by
  `src/init.rs`, parsed by `src/config`); no command sets it.
- **Pinning is a snapshot freeze.** `release` reads exports already marked
  `supported`, looks up the current snapshot id, and writes `.cell/published.json`
  (route → snapshot_id). This freeze is what makes a supported contract stable.
- **`serve` depends on the pin.** For supported routes `serve` serves the pinned
  snapshot (`serve/mod.rs:162-166`); with no pin it serves *latest*.

Promotion is already decoupled from any command. The missing piece is a command
for **deployment**. The snapshot-pinning primitive is load-bearing for the
contract guarantee and is preserved.

## Decision

Add `deploy`, rename `publish` → `release`, and keep snapshot pinning intact.

### 1. `publish` → `release` (keep the pin)

The pinning primitive stays; only the name changes, because "release" better
describes its real job: freezing the supported snapshot. `release` reads
`contract: supported` exports and writes `.cell/published.json` exactly as
`publish` does today.

`serve`'s dependency on the pin is preserved. The `Published` struct moves out of
the renamed module into a neutral location (e.g. `src/manifest.rs`) so `serve`
no longer imports from a command module, but its behavior is unchanged.

A hidden, deprecated `publish` alias remains for one release, printing:

```
publish has been renamed to `release` (it pins the supported snapshot).
Run `datamk release` instead.
```

Promotion stays what it already is: a reviewed `contract: supported` edit to
`cell.yaml`, landed through a PR. There is no `promote` command.

### 2. `datamk deploy`

`deploy` takes a cell + profile and runs it as a managed workload on an
orchestrator. It reuses the existing `FileArgs` (`-f`/`--file`, `-p`/`--profile`)
for surface consistency.

```bash
# The deploy overlay contains the target; --target can override it.
datamk deploy -f cell.yaml -p prod [--target kubernetes] [--dry-run]
```

- `--target` is a `ValueEnum` (only `kubernetes` is implemented), so `--help`
  lists valid values and rejects typos.
- `--dry-run` renders the target's manifests/artifacts to stdout without
  applying — the way to review what `deploy` will do. What gets rendered is
  target-specific.

### 3. A cell has two workloads; deploy runs both where the target supports it

Every cell has the same two runtime halves:

- the **Builder** — `datamk run`, runs on a schedule to rebuild snapshots;
- the **Server** — `datamk serve`, runs continuously to expose the interface.

A single `deploy` produces both halves where the target can host them. **How**
each target realizes "scheduled" and "long-lived" is target-specific (Kubernetes:
ADR 0002).

Not every orchestrator hosts both halves: schedulers such as Airflow and Dagster
run the Builder but have nowhere to host a long-lived Server. The backend
abstraction (§4) therefore models that a target may support only the scheduled
workload, only the long-lived workload, or both.

### 4. Pluggable backend trait

Targets sit behind a trait so orchestrators are additive — a backend is a
compile-time feature + a trait impl, not a change to the `deploy` command. The
trait:

- takes the cell **definition + artifact bundle**, not the live `engine::Cell`
  (which owns an attached DuckDB connection — deploy must not open a database as a
  side effect); and
- has each target declare which workloads it supports.

```rust
trait DeployTarget {
    /// Which of the cell's workloads this target can host.
    fn supports(&self) -> Workloads; // Scheduled | LongLived | Both

    /// Render + apply whatever runs the cell on this orchestrator.
    fn deploy(&self, cell: &CellDef, artifact: &CellArtifact, cfg: &DeployConfig)
        -> Result<DeployReport>;
}
// impls: Kubernetes (ADR 0002); Airflow/Dagster (Scheduled), Ecs (Both) — future.
```

The concrete render-and-apply mechanism is target-specific and lives in each
target's ADR (Kubernetes uses the `kube` crate behind a `kubernetes` cargo
feature — ADR 0002).

### 5. datamk ships versioned base images

CI builds and publishes freely available, versioned base images (the `datamk`
binary + runtime, cell-agnostic) to a public registry, e.g.
`ghcr.io/scalecraft-dev/datamk:vX.Y.Z`. Users do **not** build per-cell images;
the cell's content is delivered to the base image at deploy time. The image tag
defaults to the running binary's version, so a given `datamk` deploys the
matching base image. This image-supply story is shared by every container-based
target; how cell content is delivered into the image is target-specific (ADR 0002).

**Required:** DuckDB extensions (`ducklake`, `httpfs`) are **baked into the base
image**, not `INSTALL`ed at runtime — `engine::setup()` installs them on first
run, which would crashloop pods with no network egress.

### 6. Deploy topology is a tracked overlay, not part of the profile

Deploy topology (target, namespace, schedule, replicas, image, …) is **ops config
that should be tracked and PR-reviewed**. It must not live in the profile: the
profile is the one file whose schema structurally permits a literal secret
(`S3Binding.key_id`/`secret`, the catalog DSN, the principals path), which is
exactly why non-`local` profiles are gitignored. Putting tracked topology inside
a gitignored, secret-bearing file is the wrong trust boundary.

Instead, topology lives in a separate **tracked** `deploy/<name>.yaml` overlay,
keyed by the same profile name. Its schema has **no secret fields at all**, so a
credential can't be committed to it — a structural guarantee, not a discipline.

```
my_cell/
  cell.yaml            # contract, env-free                       [tracked]
  sql/
  profiles/
    local.yaml         # ./.cell paths, no secrets                [tracked]
    prod.yaml          # catalog DSN, s3 ${VAR} creds, principals  [gitignored]
  deploy/
    prod.yaml          # target + per-target topology              [tracked]
```

```yaml
# deploy/prod.yaml  (tracked, PR-reviewed, secret-free)
target: kubernetes          # only kubernetes implemented
# allow_anonymous: false    # top-level (agnostic); true ⇒ a deliberately open,
                            # unauthenticated endpoint (§8). Read without parsing
                            # any target's schema.
# target-specific fields (namespace, schedule, serve.replicas, image, …) are
# defined by the target's ADR — see ADR 0002 for the Kubernetes schema.
```

`DeployConfig` does **not** extend `Bindings`. It lives in a new
`src/config/deploy.rs` with a `DeployConfig::load(dir, profile)` reading
`deploy/<profile>.yaml`, invoked **only** by the `deploy` command — so target
backend weight (e.g. `kube`/`k8s-openapi`) and the deploy schema stay off the
`run`/`serve` parse paths entirely.

`deploy -p <name>` reads two files, split by trust boundary: `profiles/<name>.yaml`
(what the cell connects to + secrets, gitignored) and `deploy/<name>.yaml` (how/
where the workload runs, tracked). `--target` overrides `deploy/<name>.yaml`'s
`target`. There is intentionally no `deploy/local.yaml` — you don't deploy local.

### 7. Deploy-time pre-flight (fail loud, not a dead server)

`deploy` validates before applying and refuses with actionable errors. The
target-agnostic invariants every backend enforces:

- **No target** is configured and none passed — name the profile and suggest
  `--target kubernetes`.
- **Local-storage profile** — catalog/storage point at `./.cell/...` (the
  `local` profile default), which a remote workload can't reach. Deployed
  workloads need a shared object store + catalog.
- **Catalog is not concurrent-safe** — a DuckDB-file-backed `.ducklake` catalog
  holds an exclusive OS file lock even when attached read-only, so a running
  Server blocks the Builder from attaching to commit (and vice versa). Deployed
  cells require a **metadata-DB-backed** catalog (`sqlite:` or `postgres:`);
  multi-replica or multi-node deployments require **Postgres** (a SQLite file on
  shared/networked storage is not reliable across nodes).
- **Cell won't serve** — `access.shareable: false` (the default) makes `serve`
  reject every request; an empty `interface:` means there's nothing to serve.
- **Auth not safely configured** — the two `access` failure modes below.

These are **hard failures that block the deploy**, not log warnings. `serve`'s
only pre-authorize route is `/health`, which returns `ok` regardless — so a
misconfigured pod reports "ready" while being either fully all-deny or fully open.
The operator is not watching logs; deploy must refuse before anything is applied.
How a target *verifies* these (e.g. that a k8s Secret exists and parses) is in
that target's ADR.

### 8. Auth requirement for `serve`

`serve` is default-deny and loads its only secret — the principals file
(`{ "<bearer-token>": ["role"] }` JSON) — from a **path** (`Bindings.principals`
→ `serve::load_principals`). The deploy contract requires, for every target:

- The principals material is delivered as a target-native secret, **never** as a
  ConfigMap/env var or anything that lands in plaintext logs, and mounted/exposed
  at the path the profile's `principals:` names. (Kubernetes: a `Secret` mounted
  `0400` as a file — ADR 0002. No `src/serve/` change is required.)
- `deploy` **references** operator-provided secret material and validates it; it
  does not create or manage secrets by default (keeps plaintext tokens off the
  deploy/CI path). A `deploy --create-principals <local-path>` bootstrap is an
  explicit, opt-in follow-up — never the default.
- Pre-flight enforces the two failure modes as hard refusals, **split by where
  each check can actually run**: (a) `roles` non-empty ⇒ the profile must
  *configure* a `principals:` path. That is the only part verifiable from the
  deploy host — the file at that path is the in-cluster secret mount, so its
  existence and `HashMap<String, Vec<String>>` parse are enforced at runtime by
  the `load_principals` hardening below, and the named secret is verified by the
  target (Kubernetes: ADR 0002). (b) `shareable && roles.is_empty()` (an open
  endpoint) ⇒ refuse unless `allow_anonymous: true` is explicitly set at the
  **top level** of the deploy overlay — a recorded, deliberate decision, not a
  default that ships because `roles:` was left empty. (`allow_anonymous` is
  top-level, not nested under a target's `serve:`, so the target-agnostic
  pre-flight reads it without parsing any target's sub-schema.)

**Companion serve hardening (ships with this work):** `load_principals` currently
`unwrap_or_default()`s a missing/unreadable/malformed file into an empty (all-deny)
map, silently. Change it to fail loud when `principals:` is set but the file can't
be read or parsed — turning failure mode (a) into a visible crashloop rather than a
quietly-all-deny server. This is target-agnostic serve code.

### 9. Server snapshot freshness

The Server holds one read-only connection for its lifetime and queries in
autocommit — one statement per request, no open transaction. Against a
metadata-DB-backed catalog (§7) this **auto-refreshes**: every experimental
"latest" query re-reads the catalog, so the Server reflects snapshots the Builder
commits after attach, with no re-attach, refresh loop, or per-request reconnect.
Supported routes serve their pinned snapshot and are immutable by design.

Two constraints make this hold:

- The catalog is metadata-DB-backed (`sqlite:`/`postgres:`), per §7. A
  DuckDB-file `.ducklake` catalog cannot be attached by the Builder while the
  Server holds it, so no snapshot ever commits.
- A request handler **must not** open an explicit transaction (`BEGIN`). An open
  transaction pins the catalog attach and holds the metadata lock for the
  request's duration, blocking the Builder's commit ("database is locked").
  Request queries stay in autocommit. This is target-agnostic serve code.

## Consequences

- Snapshot pinning is preserved; the contract guarantee is intact. `release` is
  the same primitive under an honest name.
- `serve` no longer imports from a command module; `Published` lives in a neutral
  `manifest` module.
- CI gains a base-image build-and-publish job (on tagged releases), with
  extensions baked in.
- New backends are trait impls behind a cargo feature, declaring which workloads
  they support — not command changes. Each gets its own ADR.
- Deployed profiles must use a **metadata-DB-backed catalog** (`sqlite:`/`postgres:`;
  Postgres for multi-replica/multi-node) + shared object store — not a DuckDB-file
  `.ducklake` catalog or the embedded `local` backend. This is enforced at deploy
  time. The `datamk init` profile template and the example profiles, which default
  to a file-backed `.ducklake`, are updated so a deployable profile is the default.
- Deploy topology lives in a tracked, secret-free `deploy/<name>.yaml` overlay
  (§6); `Bindings` is not extended and `run`/`serve` parse paths are untouched.
- `serve` auth is the principals secret wired at the path the profile already
  names (§8) — **no `src/serve/` change** beyond the companion `load_principals`
  fail-loud hardening, which ships with this work.
- `datamk init` scaffolding (README, `cell.yaml` comments, `.gitignore`, a
  commented `deploy/prod.yaml`) is updated to reference `release`/`deploy`, to keep
  `deploy/` tracked, and to teach promotion-via-PR.

## Alternatives considered

- **Delete `publish` outright.** Rejected: it removes the snapshot-pinning
  mechanism `serve` depends on, breaking compilation and silently downgrading
  supported routes to "latest" — a contract regression.
- **A `promote` command alongside `deploy`.** Deferred: promotion is already a
  reviewed `cell.yaml` edit; a command would make a maintenance commitment feel
  routine.
- **Deploy config inside the profile** (`deploy:` on `Bindings`). Rejected: it puts
  tracked ops topology in a gitignored, secret-bearing file and drags deploy schema
  through every `run`/`serve` parse. A separate tracked overlay (§6) keeps the
  trust boundary intact.
- **Per-cell container images built by users.** Rejected for v1: versioned base
  images + delivered cell content avoid a build step per user and per cell.
- **Render-manifests-only v1 (no apply).** Rejected: v1 does full apply to cluster,
  with `--dry-run` covering the render-and-review use case.
