# ADR 0002 — Kubernetes deploy target

- **Status:** Proposed
- **Date:** 2026-06-29
- **Deciders:** Datamake team
- **Author:** @scottypate
- **Depends on:** ADR 0001 — `datamk deploy` (defines the command, the
  `DeployTarget` trait, the deploy overlay, base images, and the agnostic
  invariants this ADR realizes)

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

If `schedule` is omitted, only the Server is deployed (serve-only, no CronJob).
Kubernetes supports **both** workloads, so `Kubernetes::supports()` returns `Both`.

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
schedule: "0 * * * *"       # cron for the run CronJob; omit ⇒ serve-only, no CronJob
serve:
  port: 8080
  replicas: 2
  # allow_anonymous: true   # required to deploy a shareable cell with empty roles (§6)
# image:                    # omit ⇒ default to this binary's version (ADR 0001 §5)
# imagePullSecret: regcred  # a k8s Secret *name*, never the secret itself
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

### 5. Secret wiring (profile + principals)

`serve`/`run` need the profile (`profiles/<name>.yaml`), which can carry the
catalog DSN and S3 creds, and `serve` needs the principals file. Both are
secret-grade and are delivered as Kubernetes **Secrets**, never ConfigMaps:

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
- `access.shareable: true` with empty `roles` ⇒ refuse unless `serve.allow_anonymous:
  true` is set in the overlay.
- `imagePullSecret` / referenced Secrets must exist before apply.

### 7. Service exposure

**Service type is ClusterIP only** in v1. Deploy does **not** auto-provision a
LoadBalancer or Ingress — least of all for an anonymous endpoint. Public exposure
(ingress/host field) is a deliberate follow-up, not a default. The `DeployReport`
must therefore describe the route as in-cluster only and not imply a curl-able URL.

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
