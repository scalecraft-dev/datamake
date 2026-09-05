# ADR 0002 — Kubernetes deploy target

- **Status:** Proposed
- **Date:** 2026-06-29
- **Deciders:** Datamake team
- **Author:** @scottypate
- **Depends on:** ADR 0001 — `datamk deploy` (defines the command, the
  `DeployTarget` trait, the deploy overlay, base images, and the agnostic
  invariants this ADR realizes)
- **Superseded in part by ADR 0004:** the Postgres-catalog pre-flight and the
  live-catalog freshness validation are replaced by the published-catalog
  serving model (fetch-and-swap serve, publish-per-execution Builder).

## Context

ADR 0001 establishes the deploy contract and a pluggable `DeployTarget` trait but
deliberately leaves the orchestrator-specific mechanics undecided. This ADR
specifies the **first and (for now) only** implemented target: Kubernetes. It is
the `kubernetes` cargo feature and the `impl DeployTarget for Kubernetes`.

Everything here is Kubernetes-specific. The command surface, the `release`
rename, the tracked overlay concept, the base-image supply chain, and the
agnostic pre-flight invariants are all in ADR 0001 and are not repeated.

## Decision

### 1. What gets rendered

A Kubernetes `deploy` realizes the cell's two workloads (ADR 0001 §3) as:

- a **CronJob** running `datamk run` to rebuild snapshots on `schedule`, and
- a **Deployment** (+ **Service**) running `datamk serve` to expose the interface.

Both are **presence-based** in the overlay (amended by issue #8, 2026-09): the
CronJob renders iff `schedule:` is present, the Deployment + Service iff `serve:`
is present. A cell that exists only to be composed by other cells has no HTTP
consumer — composition is build-time against published artifacts, never a call
to the upstream Server — so it omits `serve:` and runs no idle Server. An overlay
with neither block is refused at schema validation (it would render only a
ConfigMap and a one-shot init Job). `serve: {}` deploys a Server with every
default; a bare `serve:` is YAML null, i.e. absent.
Kubernetes supports **both** workloads, so `Kubernetes::supports()` returns `Both`;
whether *this* deploy renders a Server is `DeployTarget::serves(overlay)`, and the
agnostic Server-only pre-flight (servable, auth — ADR 0001 §7/§8) keys on that,
not on the target's capability. Those checks are unchanged whenever a Server is
rendered.

Additionally, a `deploy` renders a one-shot **init Job** running `datamk run`, and
applies + **waits for it to complete before the Server** (order: ConfigMap → init
Job → Service/Deployment/CronJob). This closes a real bootstrap gap found by the
`kind` e2e harness: `serve` opens DuckLake `READ_ONLY`, and DuckLake refuses to
auto-create a catalog under `READ_ONLY`, so against a fresh metadata-DB catalog the
Server would crash-loop until the first Builder run initialized it. Running the
Builder as part of deploy means the catalog (and first snapshot) exist before the
Server starts, and it upgrades the deploy's success signal: a broken transform or
an unreachable catalog/store now fails the deploy **with the build pod's logs**,
not as a silent crash-loop discovered later. The init Job is content-addressed
(`<cell>-init-<hash>`, so re-deploying identical content is an idempotent no-op) with
`ttlSecondsAfterFinished` cleanup. `--skip-init` opts out (operator drives the
Builder themselves); `--init-timeout` bounds the wait (default 300s). `--dry-run`
renders the init Job like everything else but applies/waits nothing.

### 2. Apply mechanism: the `kube` crate, behind the `kubernetes` feature

Apply uses the **`kube` crate**, not a `kubectl` shell-out, gated behind a
`kubernetes` cargo feature:

- Keeps datamk a single self-contained binary — no dependency on an external
  `kubectl` on `PATH`.
- Gives in-cluster config detection, server-side apply (declarative, idempotent),
  typed objects, and structured errors instead of stderr-scraping.
- Avoids silently targeting whatever cluster an ambient `kubeconfig` context
  happens to point at.

The feature flag keeps `kube`/`k8s-openapi` compile time and binary bloat out of
the lean `run`/`serve` build. `--dry-run` (ADR 0001 §2) renders the manifests to
stdout without contacting a cluster — also the way CI validates template
correctness.

### 3. Kubernetes deploy overlay schema

The `target: kubernetes` overlay (`deploy/<name>.yaml`, ADR 0001 §6) carries:

```yaml
# deploy/prod.yaml  (tracked, PR-reviewed, secret-free)
target: kubernetes
namespace: data-prod
schedule: "0 * * * *"       # cron for the run CronJob; omit ⇒ no CronJob
# allow_anonymous: true     # top-level (agnostic, ADR 0001 §6/§8): required to deploy
                            # a shareable cell with empty roles. NOT nested under `serve:` —
                            # the target-agnostic pre-flight reads it without parsing this
                            # target's sub-schema.
serve:                      # omit ⇒ no Deployment, no Service (issue #8); `serve: {}` ⇒ defaults
  port: 8080
  replicas: 2
# image:                    # omit ⇒ default to this binary's version (ADR 0001 §5)
# imagePullSecret: regcred  # a k8s Secret *name*, never the secret itself
# serviceAccounts:          # opt-in, one identity per role (issue #14, §5); omit ⇒
#   builder: orders-builder #   pods run as the namespace default
#   server: orders-server
```

`image` is an `Option<String>` (omitted ⇒ default), not an empty-string sentinel.
No field here may carry a secret; secret material is referenced by k8s object
**name** only (e.g. `imagePullSecret`).

### 4. Cell content delivery: ConfigMap

The base image is cell-agnostic (ADR 0001 §5), so the cell's content
(`cell.yaml` + `sql/*`) is delivered at deploy time via a **ConfigMap** mounted
into both workloads at a fixed path (e.g. `/cell/`); invocations become
`datamk run -f /cell/cell.yaml -p prod` and `datamk serve -f /cell/cell.yaml -p prod`.
SQL + YAML are far under the 1 MiB ConfigMap limit.

Required discipline, because a mounted ConfigMap is otherwise a silent mutability
hole in "a cell is a contract":

- Name the ConfigMap by **content hash** and set `immutable: true`.
- Stamp a checksum annotation on the pod template so the long-lived Server
  Deployment actually **rolls** when content changes (mounted ConfigMaps update
  lazily and won't restart the process otherwise; the CronJob gets a fresh pod
  each run regardless).

The **profile does not go in the ConfigMap** — it can carry secrets. See §5.

A content-addressed artifact or git-ref pulled by an init container is the durable
alternative (immutable, auditable record of what's deployed); deferred past v1.

### 5. Secret wiring (profile + principals) and identities

`serve`/`run` need the profile (`profiles/<name>.yaml`), which can carry the
catalog DSN and S3 creds, and `serve` needs the principals file. Both are
secret-grade and are delivered as Kubernetes **Secrets**, never ConfigMaps.
Amended by issue #14 (2026-09): the pods are **two identities** — the
**Builder** (init Job + CronJob) and the **Server** (Deployment) — and the
wiring below is per identity, not per pod. See §8 for the amendment.

- **Principals** → a Secret (key `principals.json`) mounted as a `secret` volume,
  `defaultMode: 0400`, read-only, at a fixed path (e.g. `/etc/datamk/principals.json`).
  The in-cluster profile's `principals:` is set to that path. This requires **no
  change to `src/serve/`** — `serve` already loads principals from a path. An env
  var would require a code change and leak the token map into `kubectl describe` /
  the process environment.
- **Reference-by-name, operator-created.** `deploy` references the principals
  Secret (default name `<cell>-principals`) and validates it; it does **not**
  create or manage it. This keeps plaintext tokens off the deploy/CI path, needs
  no Secret-write RBAC, and composes with External Secrets / Vault / sealed-secrets.
- **Profile** → delivered as its own Secret mount (it is not in the ConfigMap).
  Deploy asserts the profile's `principals:` value equals the principals mount path.
- **Rotation:** `serve` reads principals once at startup, so rotating tokens needs
  a `kubectl rollout restart`. To make rotation roll automatically, stamp a hash of
  the Secret onto the pod template (same checksum-annotation mechanism as §4).

### 6. Kubernetes pre-flight enforcement

Realizes ADR 0001 §7–§8 against the cluster, as **hard failures that block apply**:

- `access.roles` non-empty ⇒ the named principals Secret must exist in the
  namespace, carry the `principals.json` key, parse as `HashMap<String, Vec<String>>`,
  and match the profile's `principals:` path. (Source JSON is validated even when
  operator-created, because `load_principals` swallows malformed JSON into an
  all-deny map.)
- `access.shareable: true` with empty `roles` ⇒ refuse unless top-level
  `allow_anonymous: true` is set in the overlay. (Enforced by the agnostic pre-flight,
  ADR 0001 §8 — the Kubernetes target does not re-check it.)
- `imagePullSecret` / referenced Secrets must exist before apply — each only
  where a rendered pod mounts it (§8): the Builder profile when a Builder is
  rendered, the Server profile (and principals) only when `serve:` is present.
- The Server profile Secret must parse as a profile and carry no `connections:`.
- `serviceAccounts.server` without `serve:`, or `serviceAccounts.builder` on an
  all-bound cell, is refused: an account nothing runs as is worse than none.

### 7. Service exposure

**Service type is ClusterIP only** in v1. Deploy does **not** auto-provision a
LoadBalancer or Ingress — least of all for an anonymous endpoint. Public exposure
(ingress/host field) is a deliberate follow-up, not a default. The `DeployReport`
must therefore describe the route as in-cluster only and not imply a curl-able URL.

### 8. Amendment (2026-09, issues #8 and #14): Server optional, identities split

**Server optional (#8).** §1 is amended: the Deployment + Service render iff
`serve:` is present, exactly as the CronJob renders iff `schedule:` is. A cell
that exists only to be composed has no HTTP consumer (composition is build-time
against published artifacts) and should not pay for an idle Server. Neither
block ⇒ refused at schema validation; an all-bound cell without `serve:` is
refused by the target pre-flight with `serve: {}` named as the only fix. The
agnostic Server-only pre-flight (servable, auth) keys on `DeployTarget::serves`
— "this deploy renders a Server" — not on the target's capability, and is
unchanged whenever a Server is rendered. Explicitly *not* adopted: a gateway
Deployment serving the mesh off the `mesh emit` manifest — `serve` never serves
a hosted index of cells (no-control-plane is the thesis).

**Two identities (#14).** The Server's *code* never touches a warehouse
(`connectors::prepare` is reached only from `run`/`verify`/`interface`), but
under Workload Identity its *ambient identity* was the Builder's, so an RCE in
`serve` inherited warehouse reach it never needed. Two changes:

- **Profile Secret split, unconditional.** Builder pods mount `<cell>-<profile>`
  (the full profile). The Server mounts `<cell>-<profile>-server`: the same
  `<profile>.yaml` key at the same path, but a reduced document — storage,
  `s3`/`gcs`, `principals`; **no `connections:`**. Two operator-created Secrets
  rather than one Secret with two keys: a profile is one YAML value under one
  key, volume `items:` projects keys not fields, and `deploy` never sees profile
  plaintext (§5 reference-not-create) so it cannot derive the reduced document
  itself. One Secret with a second key was rejected because an ESO/sealed-secret
  sync owning the object would silently overwrite it. The invariant — **no
  connector DSN reaches the Server pod** — is pinned twice: a render test (the
  Deployment mounts only the `-server` Secret; Builder pods only the full one)
  and a live pre-flight that parses the Server Secret and refuses `connections:`.
  Principals are now mounted into the Server only; `serve` is their sole reader.
  Honest scope: connection configs carry no literal secrets by design (a
  `password:` must be a `${VAR}`), so the split removes warehouse *coordinates*
  (host, database, user, key path — recon and targeting). The control that
  removes *reach* is the account split below.
- **Service accounts, opt-in.** `serviceAccounts: {builder, server}` in the
  overlay. When a name is set, `deploy` renders a ServiceAccount (name,
  namespace, labels — **never annotations**: the operator's IAM binding lives
  there, and a forced server-side apply would prune any field datamk claimed)
  and sets `serviceAccountName` on that role's pods; accounts are applied before
  any pod-bearing object because the admission plugin refuses a pod whose
  account is missing. When absent, pods run as the namespace default as before,
  so laptop/`kind` clusters need nothing. Under Workload Identity the operator
  binds cloud IAM to each account — one extra binding over the single-identity
  setup, not a new step.

**Loader consequence.** `config::load` used to refuse a `connection` source whose
connection is absent from the profile. The Server's reduced profile is exactly
that shape, so a missing connection now resolves to
`ResolvedSource::MissingConnection` and the same error is raised at first use —
`connectors::prepare` (every warehouse reader) and the agnostic deploy
pre-flight (so a Builder is refused on the deploy host, not in its pod). `run`
still fails before `BEGIN`; `serve` never reads a connection and starts.

## Consequences

- A `kubernetes` cargo feature pulls in `kube` + `k8s-openapi`; the default
  `run`/`serve` build is unaffected.
- The base image (ADR 0001 §5) must ship with `ducklake` + `httpfs` pre-installed,
  or pods crashloop in a no-egress cluster.
- Deployed cells require a **Postgres** catalog + `s3://` storage (ADR 0001 §7);
  the Kubernetes pre-flight is where that's enforced for this target.
- Operators own principals/profile Secrets out-of-band (ESO/Vault/sealed-secrets
  all work); `deploy` only references and validates them.
- Updating a deployed cell is re-running `datamk deploy`: server-side apply
  reconciles the ConfigMap/Secrets and the checksum annotation rolls the Server.
- The Server auto-refreshes against the shared Postgres catalog (ADR 0001 §9):
  because the CronJob and Server share one metadata-DB catalog, a CronJob commit
  becomes visible on the running Server without a restart. Acceptance criterion:
  `datamk run` completes while `serve` is up, and the experimental endpoint's row
  count increases without restarting the Server.
- `imagePullSecret` is optional — v1 base images are public (ADR 0001 §5). When set
  for a private mirror it names an existing Secret, validated at pre-flight (§6).

## Alternatives considered

- **`kubectl` shell-out for apply.** Rejected: depends on an external binary and
  ambient kubeconfig context (wrong-cluster risk) and turns error handling into
  stderr-scraping. We use the `kube` crate behind a feature flag.
- **Cell content via init-container pull (v1).** Deferred: ConfigMap is simpler for
  v1 and the size limit is a non-issue; the pull model's auditable-record benefit
  doesn't yet justify the extra moving part.
- **Principals via env var.** Rejected: requires a `serve` code change and exposes
  the token map in the process environment / `kubectl describe`. File mount changes
  no serve code.
- **`deploy`-managed principals Secret (create from a local file by default).**
  Rejected as the default: routes plaintext tokens through the deploy path/CI logs
  and needs Secret-write RBAC. Offered later as an explicit opt-in
  (`--create-principals`).
- **Auto-provisioning a LoadBalancer/Ingress.** Rejected for v1: ClusterIP keeps a
  freshly deployed (possibly anonymous) endpoint off the public network by default.
