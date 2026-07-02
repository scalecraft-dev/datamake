# kind e2e: ADR 0002 (Kubernetes deploy target)

Deploys the `orders` cell to a real, local `kind` cluster and validates it end
to end -- the render/apply/pre-flight unit tests in `src/deploy/targets/kubernetes/`
prove the manifests are *correct*; this proves they actually *work* against a
real API server, a real Postgres catalog, and a real S3-compatible store.

**This is LOCAL-ONLY. It is never run in CI** (nothing under `.github/workflows/`
touches it). It shells out to `docker`/`kind`/`kubectl`, builds a full release
Docker image (slow -- bundled DuckDB), and creates/deletes a `kind` cluster.
Run it by hand, on your machine, when you've touched anything under
`src/deploy/`.

## Prerequisites

`docker`, `kind` (>=0.20), `kubectl`, `jq`, `curl` on `PATH`, and a running
Docker daemon. `cargo` at `$HOME/.cargo/bin` (or override `DATAMK_BIN`).

## Running it

```sh
make e2e            # preflight -> up -> build -> infra -> secrets -> deploy -> validate
make e2e-down        # tear down the cluster when you're done
```

The full run leaves the cluster **running** on success so you can poke at it
(`kubectl --context kind-datamk-e2e -n datamk-e2e get pods`, etc.). Tear it
down explicitly with `make e2e-down`.

To debug a failed run without paying for a fresh cluster + image build every
time, drive individual phases directly:

```sh
./test/integrations/kind_e2e/run.sh preflight   # tool + Docker daemon check
./test/integrations/kind_e2e/run.sh up          # kind cluster + namespace
./test/integrations/kind_e2e/run.sh build       # docker build + kind load + host cargo build
./test/integrations/kind_e2e/run.sh infra       # Postgres + MinIO + bucket
./test/integrations/kind_e2e/run.sh secrets     # profile Secret
./test/integrations/kind_e2e/run.sh deploy      # datamk deploy (host binary -> kind apiserver)
./test/integrations/kind_e2e/run.sh validate    # the assertions
./test/integrations/kind_e2e/run.sh down        # delete the cluster
```

`make e2e-up`, `make e2e-deploy`, `make e2e-validate`, and `make e2e-down` wrap
the corresponding phases. Config is env-overridable (`CLUSTER`, `NAMESPACE`,
`IMAGE`, `PROFILE`, `BUCKET`, `CELL_DIR`, `LOCAL_PORT`) -- see the top of
`run.sh`.

## What this validates

This exercises the full ADR 0002 acceptance path against a real cluster:

1. **Render + apply**: `datamk deploy` (the host binary, talking to the kind
   apiserver) produces a ConfigMap (`orders-<hash>`), an init Job
   (`orders-init-<hash>`), a Service, a Deployment, and a CronJob (all named
   `orders`), applies them in that order, and the cluster actually accepts
   and starts them.
2. **Cell content delivery** (ADR 0002 §4): the ConfigMap mount really
   reproduces `/cell/cell.yaml` + `/cell/sql/*.sql` inside the pod (sanitized
   keys, restored paths via `items[].path`).
3. **Profile secret wiring** (ADR 0002 §5): the profile Secret
   (`orders-e2e`), mounted at `/cell/profiles`, really carries the catalog DSN
   and S3 creds the container needs to attach DuckLake against Postgres +
   MinIO.
4. **Deploy-time init build**: `datamk deploy` renders + applies, in order,
   ConfigMap -> **init Job (`<cell>-init-<hash>`, runs `datamk run`, applied
   and waited-to-completion)** -> Service -> Deployment -> CronJob
   (`src/deploy/targets/kubernetes/apply.rs:apply_all`). The harness confirms
   the init Job is there and `Completed` by the time `deploy` returns, and
   that `deploy --skip-init`/`--init-timeout` are understood by the CLI
   (a `--dry-run` still renders the Job either way).
5. **The freshness capstone** (ADR 0002 §9 / Consequences acceptance
   criterion), in two parts:
   - **Immediate serve (0 -> 4 rows, no manual bootstrap)**: because `deploy`
     itself already ran and waited on the init Job before ever applying the
     Service/Deployment, the DuckLake catalog and `snapshot_id: 1` (the pin
     `.cell/published.json` names) already exist the moment `deploy` returns.
     The harness rolls out `deployment/orders` and the export serves exactly
     4 rows (the deterministic `orders_daily` output: `us-east`/`us-west`/
     `eu-west` across 2 order dates) immediately -- no separate Builder run
     required. (This used to require a manual `kubectl create job
     --from=cronjob/orders` bootstrap step before the init Job existed; see
     "bugs hit" below for the crash-loop this closed.)
   - **Steady state (no restart)**: `orders_daily` is `contract: supported`,
     pinned forever to `snapshot_id: 1`, so a *second* Builder commit can't
     move what's served -- that's the point of pinning. What it proves
     instead is the literal ADR §9 claim: once the Server has attached
     successfully, a later Builder commit through the shared Postgres
     catalog must not require restarting it. The harness runs the Builder a
     second time (via `kubectl create job --from=cronjob/orders`, or the
     CronJob itself) and asserts the Server pod's name and container
     `restartCount` are unchanged, and the export still serves correctly.

## Layout

- `run.sh` -- the harness. All the complexity lives here; the Makefile targets
  are a thin dispatch to `run.sh <phase>`.
- `manifests/postgres.yaml`, `manifests/minio.yaml` -- the catalog + object
  store this cell's `e2e` profile points at. Applied with `-n <namespace>` by
  `run.sh infra` (no namespace baked into the files).
- `cell/` -- a **self-contained** copy of `test/integrations/orders/`
  (`cell.yaml`, `sql/`, `.cell/published.json`), plus an `e2e` profile
  (`cell/profiles/e2e.yaml`) and deploy overlay (`cell/deploy/e2e.yaml`) that
  point at this harness's in-cluster Postgres/MinIO. Kept separate from
  `test/integrations/orders/` on purpose -- that fixture is driven by
  `cargo test` and must not grow a dependency on a `kind` cluster existing.

## Two binaries, on purpose

- The **host** `datamk` (built via `cargo build --bin datamk`, this repo's
  native target -- macOS/arm64 in this environment) runs `datamk deploy`,
  which talks to the kind cluster's **apiserver** over the network. It never
  runs inside a pod.
- The **container image** (`docker build -t datamk:e2e .`, Linux, then
  `kind load docker-image`) is what the Deployment/CronJob pods actually run.

These are deliberately separate binaries built for deliberately different
platforms -- the host binary cannot run in the pod and vice versa. Don't try
to reuse one for the other.

The image is tagged `datamk:e2e`, not `:latest`: Kubernetes' default
`imagePullPolicy` is `IfNotPresent` for any tag other than `latest` (or none),
so the kubelet uses the image `kind load docker-image` already put on every
node and never tries to reach a registry that doesn't exist for this image.

## Real bugs hit bringing this up (and the fix)

Every one of these was invisible to the existing unit/integration test suite
(none of it touches a real Postgres, MinIO, or API server) and only surfaced
by actually applying to a `kind` cluster:

1. **`postgres://` DSNs silently didn't attach (product bug, fixed).**
   `engine::setup` never `INSTALL`ed the DuckDB `postgres` extension, and
   DuckLake's postgres catalog backend parses a *libpq* keyword/value
   connect string after `ducklake:postgres:` (`dbname=x host=y ...`), not a
   `postgres://user:pass@host/db` URL. Passing the URL straight through
   didn't error clearly -- DuckDB just tried to open a local file literally
   named `postgres:/user:pass@host/db`. Fixed in `src/engine/mod.rs`:
   `load_catalog_extension` now `INSTALL`s `postgres`/`sqlite` as needed, and
   `postgres_url_to_ducklake` translates the familiar URL form (what every
   profile in this repo, docs included, uses) into DuckLake's connect
   string at the boundary, so the contract's DSN shape doesn't change.

2. **A 64-char label value crashed `apply` outright (product bug, fixed).**
   `render_configmap` stamped the *full* SHA-256 content hash (64 chars) as
   the `datamk.io/content-hash` label **value**. Kubernetes caps label
   values at 63 bytes; a real API server rejected the ConfigMap with
   `must be no more than 63 bytes`. `--dry-run` and every render unit test
   never noticed, because they only inspect the typed struct -- nothing
   validates it against real API server limits. Fixed in
   `src/deploy/targets/kubernetes/render.rs`: the label now uses the same
   12-char prefix as the ConfigMap name (`content_hash_short`, one helper
   for both), plus a regression test asserting every rendered label value
   fits 63 bytes.

3. **`s3.endpoint` must be a bare `host:port` (fixture/profile bug, not a
   product bug).** `use_ssl`/`url_style` are separate keys; DuckDB builds
   the actual request URL as `http(s)://` + `ENDPOINT` + path, so an
   `endpoint: http://host:port` produces a doubled `http://http://host:port/...`
   that fails to resolve as a hostname. Confirmed directly with a throwaway
   probe against this harness's own MinIO: the `http://`-prefixed endpoint
   failed with exactly that doubled-URL DNS error on the PUT, and the
   bare-`host:port` form round-tripped a real Parquet file into MinIO
   (`mc ls` confirmed the object). No `src/` change needed --
   `S3Binding`/`ResolvedS3` (`src/config/schema.rs`) and
   `engine::create_s3_secret` (`src/engine/mod.rs`) already expose and wire
   `endpoint`/`url_style`/`use_ssl` correctly; `cell/profiles/e2e.yaml` just
   has to use them right.

4. **The Server used to crash-loop until the first-ever Builder run -- fixed
   in product, not patched around in this harness.** `datamk serve` opens
   DuckLake `READ_ONLY` (`src/engine/mod.rs`), and DuckLake refuses to
   auto-create a catalog under `READ_ONLY` ("Existing DuckLake ... does not
   exist - and creating a new DuckLake is explicitly disabled"). On a truly
   fresh Postgres, that used to mean the Server container didn't serve a
   graceful empty response before the first build -- it failed to start at
   all and sat in `CrashLoopBackOff` until a Builder run (CronJob or
   otherwise) created the catalog. This is exactly the finding this harness
   originally surfaced (`first-deploy ordering matters`), and it's now
   closed at the product level: `datamk deploy` renders + applies a one-shot
   init Job (`<cell>-init-<hash>`, runs `datamk run`) and waits for it to
   *complete* before the Service/Deployment/CronJob are ever applied
   (`src/deploy/targets/kubernetes/apply.rs::apply_and_wait_init`). A fresh
   deploy now self-initializes the catalog and the Server serves correctly
   from its very first rollout -- see item 4/5 above. `--skip-init` opts an
   operator who wants to drive the Builder themselves back out of this.

5. **Fixture-scale nuance: this cell's data never actually reaches S3.**
   DuckLake auto-inlines small tables directly into the Postgres catalog
   (`ducklake_default_data_inlining_row_limit` defaults to 10 rows); the
   fixture's `orders_daily` is 4 rows, so it's always inlined and never
   written to MinIO as Parquet. The deployed cell's `storage: s3://...`
   binding is still exercised (the S3 secret is created, `httpfs` loads,
   MinIO's reachability/credentials are real), but no object ever lands in
   the bucket for *this* fixture's data. Bug (3) above was independently
   confirmed against real Parquet writes via a throwaway probe outside this
   fixture, so the endpoint wiring is proven correct -- just not exercised
   by `make e2e`'s own row volume. Not something to fix here (the fixture is
   deliberately tiny and deterministic); flagging it so nobody mistakes an
   empty MinIO bucket after a green `make e2e` for a broken S3 path.

6. **Harness-only bugs (not product bugs, fixed in `run.sh`):** a `-l
   app=orders` pod selector matched both the Server's Deployment pods and
   every CronJob-spawned Job pod (same `app` label on every pod template,
   ADR 0002 §1/§4) -- `.items[0]` picked whichever sorted first, once
   silently grabbing a *completed builder Job pod* instead of the Server and
   poisoning the "same pod, no restart" comparison. Fixed with a
   `pod-template-hash`-qualified selector (only ReplicaSet-managed pods
   carry that label). Separately, backgrounding `k port-forward ... &`
   (`k` is a shell function) captured the wrapping subshell's PID in `$!`,
   not the real `kubectl` process's -- killing it didn't reliably tear down
   the actual listener, which could outlive its script phase, hold the
   local port, and keep answering from a since-deleted pod's network
   namespace. Fixed by backgrounding `kubectl` directly and adding a
   pattern-matched `pkill` as a backstop. And the original `*/5 * * * *`
   CronJob schedule raced with the harness's own manual trigger; the
   overlay now uses `0 3 1 1 *` (valid cron, never fires during a normal
   run) so `run.sh` has full control over exactly when the Builder runs.
