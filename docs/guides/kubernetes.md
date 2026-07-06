# Deploying a cell to Kubernetes

`datamk deploy` runs a cell's two workloads on a Kubernetes cluster:

- the **Builder** — a CronJob running `datamk run`, rebuilding snapshots on
  `schedule` (omit `schedule` for serve-only, no CronJob), and
- the **Server** — a Deployment (+ ClusterIP Service) running `datamk serve`,
  exposing the interface as REST + OpenAPI.

`deploy` runs on your machine and talks to the cluster through your kubeconfig
(or in-cluster config). It applies to the **namespace named in the deploy
overlay** — never the ambient kubeconfig default. The design decisions behind
all of this live in [ADR 0001](adr/0001-deploy-command.md) (the deploy contract)
and [ADR 0002](adr/0002-kubernetes-target.md) (the Kubernetes target).

## Prerequisites

1. **A `datamk` binary with the `kubernetes` feature** (on by default):

   ```bash
   cargo build --release --bin datamk
   ```

2. **A Postgres catalog + S3-compatible object store** reachable from the
   cluster. Deployed cells require a metadata-DB catalog and remote storage —
   a local `.ducklake` file or `./.cell` path is refused at pre-flight.

3. **An image the cluster can pull.** Pods run the cell-agnostic `datamk` base
   image (built from this repo's `Dockerfile`); cell content is delivered at
   deploy time, so you never build a per-cell image. Set `image:` in the deploy
   overlay to a tag you've pushed (or, on kind:
   `docker build -t datamk:dev . && kind load docker-image datamk:dev`).

## Files: profile + deploy overlay

Deploying `-p prod` reads two files, split by trust boundary:

```yaml
# profiles/prod.yaml — what the cell connects to. Gitignored: may carry secrets.
catalog: postgres://user:pass@pg.host:5432/mydb   # postgres:// required for replicas > 1
storage: s3://my-bucket/cells/mycell
s3:
  region: us-east-1
  endpoint: s3.host:9000          # bare host:port for MinIO/R2; omit for AWS
  key_id: ${AWS_ACCESS_KEY_ID}    # env-expanded — no literal secrets in the file
  secret: ${AWS_SECRET_ACCESS_KEY}
# principals: /etc/datamk/principals.json   # required iff cell.yaml sets access.roles
```

```yaml
# deploy/prod.yaml — how/where the workloads run. Tracked, PR-reviewed, secret-free.
target: kubernetes
namespace: data-prod
schedule: "0 * * * *"        # Builder cron; omit ⇒ serve-only, no CronJob
serve:
  port: 8080
  replicas: 1                # >1 requires a postgres:// catalog (enforced)
image: registry/you/datamk:tag   # omit ⇒ this binary's version tag
# imagePullSecret: regcred       # a k8s Secret *name*; only for private registries
# allow_anonymous: true          # top-level; required to deploy a shareable cell
                                 # with empty access.roles (a deliberately open endpoint)
```

## Secrets: operator-created, referenced by name

`deploy` **references** Secrets and validates them at pre-flight; it never
creates them (no plaintext tokens on the deploy/CI path, no Secret-write RBAC,
composes with External Secrets / Vault / sealed-secrets). Create them in the
target namespace before deploying:

```bash
# The profile — name must be <cell>-<profile>, key must be <profile>.yaml.
# Mounted read-only at /cell/profiles/<profile>.yaml in every pod.
kubectl -n data-prod create secret generic mycell-prod \
  --from-file=prod.yaml=profiles/prod.yaml

# Only when cell.yaml sets access.roles — name <cell>-principals, key principals.json.
# Mounted at /etc/datamk/principals.json; the profile's `principals:` must equal
# that path (pre-flight enforces it). Rotating the Secret rolls the Server.
kubectl -n data-prod create secret generic mycell-principals \
  --from-file=principals.json=principals.json
```

## Deploy

```bash
# Review the rendered manifests without touching a cluster (also the CI check):
datamk deploy -f cell.yaml -p prod --dry-run

# Apply for real:
datamk deploy -f cell.yaml -p prod
```

A real apply is server-side apply (declarative, idempotent — re-running `deploy`
reconciles), in this order:

1. **ConfigMap** — the cell's content (`cell.yaml`, `sql/*`, the release pin),
   content-hash-named and immutable; a content change rolls the Server.
2. **Init Job** — a one-shot `datamk run`, **waited to completion**. This
   initializes the DuckLake catalog and builds the first snapshot before the
   Server starts, so `serve`'s read-only attach never races an uninitialized
   catalog. A broken transform or unreachable catalog/store fails the deploy
   here, loudly, **with the build pod's logs**.
3. **Service, Deployment, CronJob.**

Exit 0 means every object was applied and the init build completed. Flags:
`--skip-init` (you drive the Builder yourself), `--init-timeout <secs>`
(default 300), `--target kubernetes` (override the overlay's `target:`).

## Reaching the Server

The Service is **ClusterIP only** — deploy never provisions a LoadBalancer or
Ingress, least of all for a possibly-anonymous endpoint. From your machine:

```bash
kubectl -n data-prod port-forward svc/mycell 8080:8080
curl localhost:8080/openapi.json
```

`deploy` does not watch rollout health (that's cluster state, not datamk's):
`kubectl -n data-prod rollout status deploy/mycell`.

Because the Builder and Server share one Postgres catalog, a Builder commit is
visible on the running Server **without a restart** — no redeploy needed for
fresh data on experimental routes. Supported routes serve their pinned snapshot
(see `datamk release`).

## What pre-flight refuses (before anything is applied)

- Local storage or a non-metadata-DB catalog; `replicas > 1` without `postgres://`.
- `shareable: false` (the Server would deny everything) or an empty `interface:`.
- `access.roles` set but: no `principals:` in the profile, the path doesn't equal
  the in-cluster mount, the `<cell>-principals` Secret is missing, or its JSON
  doesn't parse (validated with the same parser `serve` uses at startup).
- A shareable cell with empty `roles` and no `allow_anonymous: true` — an open
  endpoint must be a recorded, deliberate decision.
- A referenced `imagePullSecret` or profile Secret that doesn't exist.

## A complete working example

`test/integrations/kind_e2e/` deploys a real cell to a local kind cluster with
in-cluster Postgres + MinIO and validates it end-to-end (`make e2e`; local-only,
requires docker + kind). Its `cell/profiles/e2e.yaml` and `cell/deploy/e2e.yaml`
are ready-to-adapt templates for everything above.
