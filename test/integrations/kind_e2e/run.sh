#!/usr/bin/env bash
# LOCAL-ONLY end-to-end harness for ADR 0002 (the Kubernetes deploy target).
#
# Deploys the `orders` cell (test/integrations/kind_e2e/cell/ -- a
# self-contained copy of test/integrations/orders/, so the unit-test fixture
# stays untouched) to a real `kind` cluster and proves the Consequences
# acceptance criterion: a `datamk run` commit becomes visible on the *same*,
# still-running `datamk serve` pod without a restart.
#
# This is NOT part of CI (see .github/workflows/) and never should be: it
# shells out to docker/kind/kubectl, builds a release Docker image (slow --
# bundled DuckDB), and mutates local Docker/kind state. Run it by hand:
#
#   make e2e          # full run: cluster up -> build -> deploy -> validate
#   make e2e-down     # tear down
#
# or drive individual phases (see `usage` below) to debug a failed run without
# paying for a fresh cluster + image build every time.
#
# All the complexity lives HERE, in one script. The Makefile targets below are
# a thin dispatch to `run.sh <phase>` -- nothing else.
set -euo pipefail

# --- config (all overridable via env) ---------------------------------------
CLUSTER="${CLUSTER:-datamk-e2e}"
NAMESPACE="${NAMESPACE:-datamk-e2e}"
IMAGE="${IMAGE:-datamk:e2e}"
PROFILE="${PROFILE:-e2e}"
BUCKET="${BUCKET:-datamk-e2e}"
# The profile Secret name deploy renders/checks for is `<cell>-<profile>`
# (render::profile_secret_name) -- the cell here is named "orders" (cell.yaml).
PROFILE_SECRET="orders-${PROFILE}"
LOCAL_PORT="${LOCAL_PORT:-18080}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
CELL_DIR="${CELL_DIR:-$HERE/cell}"
KUBECONFIG_FILE="$HERE/.kubeconfig"
DATAMK_BIN="${DATAMK_BIN:-$REPO_ROOT/target/debug/datamk}"

# Every kubectl/datamk-deploy call in this script must go through this
# kubeconfig, not whatever context happens to be ambient -- ADR 0002 §2's
# "wrong-cluster risk" is exactly what a hand-run e2e script could otherwise
# hit on a laptop with a real cluster context set.
export KUBECONFIG="$KUBECONFIG_FILE"
KCTX="kind-${CLUSTER}"

log()  { printf '\n\033[1;36m==> %s\033[0m\n' "$*" >&2; }
warn() { printf '\033[1;33mWARN: %s\033[0m\n' "$*" >&2; }
die()  { printf '\033[1;31mERROR: %s\033[0m\n' "$*" >&2; exit 1; }

k() { kubectl --context "$KCTX" -n "$NAMESPACE" "$@"; }

usage() {
  cat <<EOF
Usage: $(basename "$0") <phase>

Phases (each re-runnable on its own):
  preflight   Check required tools + a live Docker daemon.
  up          Create the kind cluster + namespace.
  build       Build the datamk:e2e image, load it into kind, build the host binary.
  infra       Apply MinIO, wait for readiness, create the S3 bucket.
  secrets     Create/refresh the profile Secret ($PROFILE_SECRET) in-cluster.
  deploy      Run the HOST datamk against the kind cluster (datamk deploy).
  validate    Assert the deployed cell actually works (see README.md).
  down        Delete the kind cluster.
  all         preflight -> up -> build -> infra -> secrets -> deploy -> validate.

Config (env overrides): CLUSTER=$CLUSTER NAMESPACE=$NAMESPACE IMAGE=$IMAGE
PROFILE=$PROFILE BUCKET=$BUCKET CELL_DIR=$CELL_DIR
EOF
}

# --- preflight ---------------------------------------------------------------

phase_preflight() {
  log "preflight: checking required tools"
  local missing=0
  need() {
    local bin="$1" hint="$2"
    if ! command -v "$bin" >/dev/null 2>&1; then
      warn "missing '$bin' -- $hint"
      missing=1
    fi
  }
  need docker "install Docker Desktop (or colima) -- https://www.docker.com/products/docker-desktop"
  need kind "brew install kind"
  need kubectl "brew install kubectl"
  need jq "brew install jq"
  need curl "should ship with macOS; install via 'brew install curl' otherwise"
  if [ "$missing" -ne 0 ]; then
    die "install the tools listed above, then re-run '$(basename "$0") preflight'"
  fi
  if ! docker info >/dev/null 2>&1; then
    die "Docker daemon is not reachable (is Docker Desktop / colima running?)"
  fi
  log "preflight OK"
}

# --- up ------------------------------------------------------------------

phase_up() {
  log "up: creating kind cluster '$CLUSTER' (if absent)"
  if kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
    log "cluster '$CLUSTER' already exists, reusing it"
  else
    kind create cluster --name "$CLUSTER" --wait 120s
  fi
  kind export kubeconfig --name "$CLUSTER" --kubeconfig "$KUBECONFIG_FILE"
  kubectl --context "$KCTX" get ns "$NAMESPACE" >/dev/null 2>&1 \
    || kubectl --context "$KCTX" create ns "$NAMESPACE"
  log "up OK (namespace '$NAMESPACE' on cluster '$CLUSTER')"
}

# --- build ------------------------------------------------------------------

phase_build() {
  log "build: docker build -t $IMAGE (this is the slow one -- bundled DuckDB, first run only)"
  docker build -t "$IMAGE" -f "$REPO_ROOT/Dockerfile" "$REPO_ROOT"

  log "build: kind load docker-image $IMAGE --name $CLUSTER"
  kind load docker-image "$IMAGE" --name "$CLUSTER"

  log "build: cargo build --bin datamk (host binary; runs 'datamk deploy' against kind's apiserver from the host -- it is NOT the binary that runs inside the pods)"
  export PATH="$HOME/.cargo/bin:$PATH"
  ( cd "$REPO_ROOT" && cargo build --bin datamk )
  [ -x "$DATAMK_BIN" ] || die "expected host binary at $DATAMK_BIN after cargo build"
  log "build OK"
}

# --- infra -------------------------------------------------------------------

wait_rollout() {
  local kind_res="$1"
  log "waiting for $kind_res rollout"
  k rollout status "$kind_res" --timeout=180s || {
    warn "$kind_res did not become ready in time -- dumping diagnostics"
    k describe "$kind_res" || true
    k get pods -l "app=${kind_res#deployment/}" || true
    k logs -l "app=${kind_res#deployment/}" --all-containers --tail=200 || true
    die "$kind_res rollout failed"
  }
}

phase_infra() {
  log "infra: applying MinIO (ADR 0004: the object store is the cell's ONLY external dependency)"
  k apply -f "$HERE/manifests/minio.yaml"
  wait_rollout deployment/minio

  log "infra: creating S3 bucket '$BUCKET' in MinIO (idempotent mc job)"
  # Throwaway minio/mc Job rather than a static manifest -- it's a one-shot
  # action, not a standing workload, and the whole point of ADR-style
  # discipline is keeping this kind of one-off out of a tracked manifest file.
  k delete job mc-bucket-init --ignore-not-found >/dev/null
  cat <<EOF | k apply -f -
apiVersion: batch/v1
kind: Job
metadata:
  name: mc-bucket-init
spec:
  backoffLimit: 3
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: mc
          image: minio/mc:latest
          command:
            - /bin/sh
            - -c
            - |
              set -e
              mc alias set local http://minio.${NAMESPACE}.svc.cluster.local:9000 minioadmin minioadmin
              mc mb --ignore-existing local/${BUCKET}
EOF
  k wait --for=condition=complete job/mc-bucket-init --timeout=120s || {
    warn "bucket-init job did not complete -- dumping logs"
    k logs job/mc-bucket-init || true
    die "S3 bucket '$BUCKET' setup failed"
  }
  log "infra OK (bucket '$BUCKET' ready)"
}

# --- secrets -------------------------------------------------------------------

phase_secrets() {
  log "secrets: creating/refreshing Secret '$PROFILE_SECRET' from $CELL_DIR/profiles/$PROFILE.yaml"
  # The pod mounts this Secret's *entire* data set at /cell/profiles (no
  # `items` filtering -- see render.rs's profile_volume), so the key here MUST
  # be exactly "<profile>.yaml" -- that's the file `config::load` looks for
  # once `--profile e2e` resolves inside the container.
  kubectl --context "$KCTX" -n "$NAMESPACE" create secret generic "$PROFILE_SECRET" \
    --from-file="${PROFILE}.yaml=$CELL_DIR/profiles/${PROFILE}.yaml" \
    --dry-run=client -o yaml | k apply -f -
  log "secrets OK"
}

# --- deploy -------------------------------------------------------------------

phase_deploy() {
  log "deploy: --dry-run sanity check for --skip-init (render-only, no apply -- doesn't touch the cluster)"
  # Light coverage for the --skip-init flag per the harness spec: we don't
  # stand up a second cluster run just to prove skip-init's runtime behavior
  # (apply.rs already has unit/integration coverage for that); a --dry-run
  # still showing the rendered init Job even with --skip-init passed is
  # enough to confirm the flag parses and deploy doesn't choke on it.
  local dry_run_out
  dry_run_out="$("$DATAMK_BIN" deploy --file "$CELL_DIR/cell.yaml" --profile "$PROFILE" --dry-run --skip-init --init-timeout 30)" \
    || die "'datamk deploy --dry-run --skip-init' failed"
  echo "$dry_run_out" | grep -q "kind: Job" || die "--dry-run output has no rendered init Job"
  echo "$dry_run_out" | grep -Eq "name: orders-init-[0-9a-f]{12}" || die "--dry-run output has no orders-init-<hash> Job name"
  log "--skip-init / --init-timeout flags OK (understood by the CLI, init Job still rendered under --dry-run)"

  log "deploy: running the HOST datamk binary against the kind cluster"
  [ -x "$DATAMK_BIN" ] || die "host binary not found at $DATAMK_BIN -- run '$(basename "$0") build' first"
  # KUBECONFIG is already exported at the top of this script; `datamk deploy`
  # uses `kube::Client::try_default()`, which honors it exactly like kubectl.
  # No --skip-init here: this is the real path every fresh cluster takes --
  # deploy applies-and-waits on the init Job itself (apply::apply_and_wait_init)
  # before the Service/Deployment are ever applied, so the DuckLake catalog +
  # snapshot 1 already exist by the time this command returns.
  "$DATAMK_BIN" deploy --file "$CELL_DIR/cell.yaml" --profile "$PROFILE"
  log "deploy OK (init build completed as part of deploy -- see phase_validate for confirmation)"
}

# --- validate ------------------------------------------------------------------

# `-l app=orders` alone matches BOTH the Server's Deployment pods AND every
# pod the CronJob's Jobs spawn (render.rs stamps the same `app: <cell>` label
# on every pod template, ADR 0002 §1/§4). Only ReplicaSet-managed pods (i.e.
# the Server) carry `pod-template-hash` -- found the hard way: `.items[0]`
# on the bare selector picked a *completed builder Job pod* instead of the
# Server (its name happened to sort first), silently poisoning the "same pod,
# no restart" comparison below. Always select the Server through this.
SERVER_SELECTOR="app=orders,pod-template-hash"

CURL_PID=""
PORT_FORWARD_LOG="/tmp/datamk-e2e-port-forward.log"
# A distinctive, greppable pattern for `pkill -f`. Matched against the full
# command line kubectl actually execs (see `start_port_forward`).
PORT_FORWARD_PATTERN="port-forward svc/orders ${LOCAL_PORT}:8080 --context=${KCTX} --namespace=${NAMESPACE}"

cleanup_port_forward() {
  if [ -n "$CURL_PID" ]; then
    kill "$CURL_PID" 2>/dev/null || true
    wait "$CURL_PID" 2>/dev/null || true
  fi
  # Belt-and-suspenders: backgrounding `k port-forward ... &` (`k` is a shell
  # *function*) puts the SUBSHELL's pid in `$!`, not kubectl's -- `kill
  # "$CURL_PID"` doesn't reliably reach the actual listening process. Found
  # the hard way: a killed subshell can leave the real `kubectl port-forward`
  # child running, still bound to a now-deleted pod's network namespace,
  # which then blocks the *next* bind attempt with "address already in use"
  # while silently serving stale/broken responses in the meantime. Belt this
  # with a pattern-matched pkill so nothing outlives its script phase.
  pkill -f "$PORT_FORWARD_PATTERN" 2>/dev/null || true
  CURL_PID=""
}

# (Re)start `kubectl port-forward svc/orders` in the background. `kubectl
# port-forward` pins to whichever pod backed the Service when it started and
# does not follow a replacement pod -- irrelevant to the happy path now that
# deploy's init Job means the Server pod never gets bounced mid-`validate`,
# but `ensure_reachable`/`start_port_forward` stay reusable this way for
# anyone re-running `validate` against a Server pod that got replaced by
# something else (an eviction, a manual delete, etc.) between phases.
#
# Calls `kubectl` directly (not the `k` wrapper) so `$!` is the real listening
# process's pid, not a subshell's -- see `cleanup_port_forward`. Returns
# non-zero on a failed bind (e.g. the target pod is `Pending`/crash-looping --
# `kubectl port-forward` refuses to attach to anything but a Running pod).
# Callers MUST NOT call this as a bare statement under `set -e` (a non-zero
# return from a bare statement kills the whole script); always go through
# `ensure_reachable` below, which is written to tolerate exactly this.
start_port_forward() {
  cleanup_port_forward
  kubectl port-forward "svc/orders" "${LOCAL_PORT}:8080" "--context=${KCTX}" "--namespace=${NAMESPACE}" \
    >"$PORT_FORWARD_LOG" 2>&1 &
  CURL_PID=$!
  local waited=0
  while ! grep -q "Forwarding from" "$PORT_FORWARD_LOG" 2>/dev/null; do
    if ! kill -0 "$CURL_PID" 2>/dev/null; then
      return 1
    fi
    waited=$((waited + 1))
    [ "$waited" -ge 10 ] && return 1
    sleep 1
  done
  return 0
}

# Keep (re)trying `start_port_forward` + `GET /` until it succeeds or
# $timeout elapses. This is the ONE thing every caller in `phase_validate`
# should use -- it tolerates the Server pod being `Pending` or mid
# crash-loop the whole time it retries (both are real, expected states
# here, not failures), and it never lets a single failed bind attempt
# propagate as a bare non-zero return under `set -e` (see `start_port_forward`).
ensure_reachable() {
  local timeout="${1:-30}"
  local waited=0
  while true; do
    if start_port_forward && curl -fsS -o /dev/null "http://127.0.0.1:${LOCAL_PORT}/" 2>/dev/null; then
      return 0
    fi
    waited=$((waited + 3))
    [ "$waited" -ge "$timeout" ] && return 1
    sleep 3
  done
}

row_count() {
  # $1 = JSON array body. Returns 0 (not a valid array, e.g. an error string)
  # rather than failing, so callers can just compare counts.
  echo "$1" | jq 'if type == "array" then length else 0 end' 2>/dev/null || echo 0
}

server_pod() { k get pods -l "$SERVER_SELECTOR" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true; }
server_restarts() {
  local pod="$1"
  k get pod "$pod" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null || true
}

# Trigger one Builder run from the CronJob and wait for it to complete,
# surfacing logs on failure. Named uniquely per call (date +%s) so this is
# safe to call more than once per validate run.
run_builder_once() {
  local job_name="orders-manual-$(date +%s)-$$"
  k create job "$job_name" --from=cronjob/orders >&2
  if ! k wait --for=condition=complete "job/$job_name" --timeout=180s; then
    warn "builder job '$job_name' did not complete -- dumping diagnostics"
    k describe "job/$job_name" || true
    k logs "job/$job_name" --all-containers --tail=200 || true
    die "builder job '$job_name' failed"
  fi
  echo "$job_name"
}

phase_validate() {
  trap cleanup_port_forward EXIT

  log "validate: confirming rendered objects exist"
  local cm
  cm="$(k get configmap -l app=orders -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)"
  [[ "$cm" =~ ^orders-[0-9a-f]{12}$ ]] || die "expected a ConfigMap named orders-<12-hex-hash>, got: '${cm:-<none>}'"
  k get service orders >/dev/null || die "Service 'orders' not found"
  k get deployment orders >/dev/null || die "Deployment 'orders' not found"
  k get cronjob orders >/dev/null || die "CronJob 'orders' not found"
  log "rendered objects OK (ConfigMap=$cm, Service=orders, Deployment=orders, CronJob=orders)"

  # --- the init Job already did the bootstrap -------------------------------
  # `datamk deploy` now applies-and-waits-on a one-shot init Job
  # (`<cell>-init-<hash>`, runs `datamk run`) BEFORE the Service/Deployment
  # ever get applied (src/deploy/targets/kubernetes/apply.rs:apply_all). By
  # the time `deploy` returned, the DuckLake catalog and snapshot 1 already
  # exist -- there is no more "Server crash-loops until someone runs the
  # Builder by hand" window to work around here. Confirm the init Job that
  # `deploy` drove is actually there and Completed.
  log "validate: confirming the init Job ran to completion"
  # kubectl's jsonpath doesn't support JSONPath filter expressions
  # (`?(@...)`) -- pull the Job list as JSON and let jq do the filtering.
  local init_job
  init_job="$(k get job -l app=orders -o json \
    | jq -r '[.items[] | select(.metadata.name | test("^orders-init-[0-9a-f]{12}$")) | select(.status.succeeded == 1)][0].metadata.name // empty')"
  [ -n "$init_job" ] || die "expected a completed 'orders-init-<12-hex-hash>' Job (deploy's init build) -- none found"
  log "init Job OK: $init_job (Completed)"

  log "validate: waiting for deploy/orders to roll out (Server applied only after the init Job above completed)"
  wait_rollout deployment/orders

  local pod_before restarts_before
  pod_before="$(server_pod)"
  [ -n "$pod_before" ] || die "no Server pod found after rollout"
  restarts_before="$(server_restarts "$pod_before")"
  log "Server pod: $pod_before (restartCount=$restarts_before)"

  local base="http://127.0.0.1:${LOCAL_PORT}"
  ensure_reachable 60 || {
    warn "port-forward never came up -- log follows"
    cat "$PORT_FORWARD_LOG" || true
    die "could not reach the Server through port-forward"
  }

  log "validate: GET / (health; published mode reports the served execution)"
  local health
  health="$(curl -fsS "$base/")" || die "GET / failed"
  echo "$health" | jq -e '.status == "ok"' >/dev/null || die "health response was not ok: $health"
  # The init build published execution 1 and the Server fetched it (ADR 0004 §6).
  echo "$health" | jq -e '.execution == 1' >/dev/null \
    || die "expected the Server to report execution 1 after the init build, got: $health"

  log "validate: GET /openapi.json, discovering the export route"
  local openapi route
  openapi="$(curl -fsS "$base/openapi.json")" || die "GET /openapi.json failed"
  route="$(echo "$openapi" | jq -r '.paths | keys[0]')"
  [ -n "$route" ] && [ "$route" != "null" ] || die "openapi.json has no routes"
  log "discovered route: $route"

  log "validate: GET $route (expect 200 + exactly 4 rows -- deploy's init build already produced snapshot 1)"
  local post_status post_body post_rows
  post_status="$(curl -s -o /tmp/datamk-e2e-post.json -w '%{http_code}' "$base$route")"
  post_body="$(cat /tmp/datamk-e2e-post.json)"
  post_rows="$(row_count "$post_body")"
  [ "$post_status" = "200" ] || die "GET $route returned HTTP $post_status: $post_body"
  [ "$post_rows" = "4" ] || die "GET $route returned $post_rows rows, want 4: $post_body"

  log "validate: checking the 4 rows look like orders_daily (us-east/us-west/eu-west across 2 dates)"
  local regions dates
  regions="$(echo "$post_body" | jq -r '[.[].region] | sort | join(",")')"
  dates="$(echo "$post_body" | jq -r '[.[].order_date] | unique | length')"
  [ "$regions" = "eu-west,us-east,us-east,us-west" ] \
    || die "unexpected regions in served rows: $regions (body: $post_body)"
  [ "$dates" = "2" ] || die "expected 2 distinct order_date values, got $dates (body: $post_body)"
  log "row content OK (regions=$regions, distinct dates=$dates)"

  # --- steady-state freshness capstone (ADR 0004 §6) ------------------------
  # An execution is not a version: a second Builder run publishes execution 2
  # to the object store, and the ALREADY-RUNNING Server must pick it up
  # through its LATEST poll (fetch-and-swap) -- same pod, no restart, no
  # rollout. `orders_daily` is `contract: supported`, pinned to snapshot 1, so
  # what it *serves* must not move (that's the point of pinning); what must
  # move is the reported execution number on `/`.
  log "validate: freshness capstone -- publish execution 2, same Server pod picks it up, no restart"
  run_builder_once >/dev/null
  log "second builder run completed (execution 2 published)"

  # The overlay sets serve.poll_interval: 5 -- the swap should land within a
  # couple of poll cycles.
  local waited=0 served_exec=""
  while [ "$waited" -lt 60 ]; do
    served_exec="$(curl -fsS "$base/" 2>/dev/null | jq -r '.execution // empty' || true)"
    [ "$served_exec" = "2" ] && break
    sleep 3
    waited=$((waited + 3))
  done
  [ "$served_exec" = "2" ] \
    || die "Server never advanced to execution 2 via its LATEST poll (still: '${served_exec:-none}')"
  log "Server advanced to execution 2 via fetch-and-swap (no restart required)"

  local pod_after restarts_after
  pod_after="$(server_pod)"
  restarts_after="$(server_restarts "$pod_after")"
  [ "$pod_after" = "$pod_before" ] \
    || die "Server pod changed ($pod_before -> $pod_after) across a routine execution -- ADR 0004 requires it NOT restart"
  [ "$restarts_after" = "$restarts_before" ] \
    || die "Server pod restartCount changed ($restarts_before -> $restarts_after) across a routine execution"

  local reroute_status reroute_body reroute_rows
  reroute_status="$(curl -s -o /tmp/datamk-e2e-reroute.json -w '%{http_code}' "$base$route")"
  reroute_body="$(cat /tmp/datamk-e2e-reroute.json)"
  reroute_rows="$(row_count "$reroute_body")"
  [ "$reroute_status" = "200" ] && [ "$reroute_rows" = "4" ] \
    || die "export broke after the second builder run: status=$reroute_status rows=$reroute_rows"

  cleanup_port_forward
  trap - EXIT

  cat <<EOF

============================================================
 PASS: kind e2e validation (ADR 0002 Kubernetes deploy target)
============================================================
  cluster:            $CLUSTER (namespace $NAMESPACE)
  route:              $route
  init build:         $init_job Completed -- deploy applied-and-waited on it
                       BEFORE the Server, so the catalog + snapshot 1 already
                       existed the moment deploy returned.
  immediate serve:    status=200 rows=4 (regions=$regions, dates=$dates), no
                       manual bootstrap Builder run required.
  server pod:         $pod_before (restartCount=$restarts_before, stable)
  2nd execution:      same pod ($pod_after), same restartCount ($restarts_after),
                       served execution advanced 1 -> 2 via the LATEST poll,
                       still status=200 rows=4 after re-querying $route

  The already-running Server picked up a second published execution from the
  object store via fetch-and-swap, WITHOUT a restart and WITHOUT any shared
  database -- the ADR 0004 acceptance criterion, verified against a real
  cluster. The cell's only external dependency is the MinIO bucket.

  Deploy's init Job (src/deploy/targets/kubernetes/apply.rs::apply_and_wait_init)
  closes the READ_ONLY bootstrap gap this harness previously worked around by
  hand: datamk serve opens DuckLake READ_ONLY, and DuckLake refuses to
  auto-create a catalog under READ_ONLY, so the Server would otherwise
  crash-loop until *some* Builder run happened. Now that run is part of
  datamk deploy itself. See test/integrations/kind_e2e/README.md.
============================================================
EOF
}

# --- down --------------------------------------------------------------------

phase_down() {
  log "down: deleting kind cluster '$CLUSTER'"
  if kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
    kind delete cluster --name "$CLUSTER"
  else
    log "cluster '$CLUSTER' does not exist, nothing to do"
  fi
  rm -f "$KUBECONFIG_FILE"
  log "down OK"
}

# --- all -----------------------------------------------------------------

phase_all() {
  phase_preflight
  phase_up
  phase_build
  phase_infra
  phase_secrets
  phase_deploy
  phase_validate
  cat <<EOF

The cluster '$CLUSTER' is still running so you can poke at it
(KUBECONFIG=$KUBECONFIG_FILE, context $KCTX).

Tear it down with:
  make e2e-down
EOF
}

# --- dispatch ------------------------------------------------------------

case "${1:-}" in
  preflight) phase_preflight ;;
  up)        phase_up ;;
  build)     phase_build ;;
  infra)     phase_infra ;;
  secrets)   phase_secrets ;;
  deploy)    phase_deploy ;;
  validate)  phase_validate ;;
  down)      phase_down ;;
  all)       phase_all ;;
  *)         usage; exit 1 ;;
esac
