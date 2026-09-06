# Deploying to Kubernetes

`datamk deploy -p <profile>` runs a cell's two workloads on a cluster: the
**Builder** (an init Job + a CronJob running `datamk run`, present iff
`schedule:` is set) and the **Server** (a Deployment + ClusterIP Service
running `datamk serve`, present iff `serve:` is set). At least one is required.
It applies to the namespace named in the overlay via your kubeconfig or
in-cluster config.

## Prerequisites

- A `datamk` built with the `kubernetes` feature (on by default).
- A Postgres catalog and an S3-compatible object store reachable from the
  cluster. Local `.ducklake` / `./.cell` paths are refused at pre-flight.
- An image the cluster can pull, built from this repo's `Dockerfile`. It is
  cell-agnostic (cell content ships in a ConfigMap) and sets `DATAMK_LOG=off`
  (pod stderr is the log pipeline). Bake connector extensions and the Snowflake
  ADBC driver into it; nothing is fetched at pod start. On kind:
  `docker build -t datamk:dev . && kind load docker-image datamk:dev`.

## Files

```yaml
# profiles/prod.yaml — gitignored; connections and creds
catalog: postgres://user:pass@pg.host:5432/mydb   # postgres:// required for replicas > 1
storage: s3://my-bucket/cells/mycell
s3:
  region: us-east-1
  endpoint: s3.host:9000          # bare host:port for MinIO/R2; omit for AWS
  key_id: ${AWS_ACCESS_KEY_ID}
  secret: ${AWS_SECRET_ACCESS_KEY}
# principals: /etc/datamk/principals.json   # required iff cell.yaml sets access.roles
```

```yaml
# deploy/prod.yaml — tracked, secret-free
target: kubernetes
namespace: data-prod
schedule: "0 * * * *"
serve:
  port: 8080
  replicas: 1
image: registry/you/datamk:tag
```

| Key | Default | Meaning |
| --- | --- | --- |
| `target` | — | `kubernetes`. Overridable with `--target`. |
| `namespace` | `default` | Namespace every object is applied to. |
| `schedule` | absent | Builder cron. Absent ⇒ no CronJob. |
| `retention_days` | `30` | `--retention-days` passed to the Builder; `0` disables compaction. |
| `serve` | absent | Absent ⇒ no Deployment/Service. `serve: {}` ⇒ all defaults. A bare `serve:` (null) counts as absent. |
| `serve.port` | `8080` | Container and Service port. |
| `serve.replicas` | `1` | Each replica holds its own catalog copy. |
| `serve.poll_interval` | `15` | Seconds between LATEST-pointer checks; must be non-zero. |
| `image` | this binary's version tag | Image for every pod. |
| `imagePullSecret` | absent | Name of an existing Secret, for private registries. |
| `serviceAccounts.builder` / `.server` | absent | ServiceAccount names; see Identities. |
| `allow_anonymous` | `false` | Required to deploy a shareable cell with empty `access.roles`. |

## Secrets

`deploy` references Secrets by name and validates them at pre-flight; it never
creates them. Create them in the target namespace first:

```bash
# Builder profile: name <cell>-<profile>, key <profile>.yaml. The full profile.
kubectl -n data-prod create secret generic mycell-prod --from-file=prod.yaml=profiles/prod.yaml
# Server profile (only with serve:): name <cell>-<profile>-server, same key.
# The profile WITHOUT connections: — pre-flight refuses one that carries them.
kubectl -n data-prod create secret generic mycell-prod-server --from-file=prod.yaml=profiles/prod-server.yaml
# Only with access.roles: name <cell>-principals, key principals.json.
kubectl -n data-prod create secret generic mycell-principals --from-file=principals.json=principals.json
```

Profiles mount read-only at `/cell/profiles/<profile>.yaml`; principals at
`/etc/datamk/principals.json`, which the profile's `principals:` must equal.
Rotating the principals Secret rolls the Server.

## Identities

| Role | Pods | Mounts | Runs as |
| --- | --- | --- | --- |
| Builder | init Job, CronJob | `<cell>-<profile>` | `serviceAccounts.builder`, else namespace default |
| Server | Deployment | `<cell>-<profile>-server`, `<cell>-principals` | `serviceAccounts.server`, else namespace default |

Named ServiceAccounts are rendered with name, namespace, and labels only; bind
cloud IAM (Workload Identity) to them yourself.

## Deploy

```bash
datamk deploy -f cell.yaml -p prod --dry-run   # render manifests; the CI check
datamk deploy -f cell.yaml -p prod             # server-side apply, idempotent
```

Apply order: ServiceAccounts → ConfigMap (cell content, content-hash-named,
immutable; a change rolls the Server) → init Job (`datamk run`, waited to
completion, build-pod logs on failure) → Service, Deployment, CronJob.
Exit 0 means every object applied and the init build finished.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--dry-run` | off | Render only. |
| `--skip-init` | off | Don't run the init Job. |
| `--init-timeout <secs>` | `300` | Wait for the init Job. |
| `--target kubernetes` | overlay's `target:` | Override the target. |

## Pre-flight refuses

- Local storage, a non-metadata-DB catalog, or `replicas > 1` without `postgres://`.
- Neither `serve:` nor `schedule:`; an all-bound cell without `serve:`.
- With `serve:`: `shareable: false`, an empty `interface:`, or empty `access.roles` without `allow_anonymous: true`.
- `access.roles` set but `principals:` missing, not equal to the mount path, the `<cell>-principals` Secret missing, or its JSON invalid.
- A missing `imagePullSecret`, missing profile Secret, or a Server Secret carrying `connections:`.
- `serviceAccounts.server` without `serve:`; `serviceAccounts.builder` on an all-bound cell.
- A `connection` source with no matching connection in the profile.

## Reaching the Server

The Service is ClusterIP only; no LoadBalancer or Ingress is created.
`deploy` does not watch rollout health. Keep `--drain-timeout` under the pod's
`terminationGracePeriodSeconds` (30 s default).

```bash
kubectl -n data-prod port-forward svc/mycell 8080:8080 && curl localhost:8080/openapi.json
kubectl -n data-prod rollout status deploy/mycell
```

Builder commits are visible on the running Server without a restart.
Supported routes serve their `datamk release` pin.

Working example: `test/integrations/kind_e2e/` (`make e2e`; needs docker + kind).
Design rationale: [ADR 0001](../adr/0001-deploy-command.md), [ADR 0002](../adr/0002-kubernetes-target.md).
