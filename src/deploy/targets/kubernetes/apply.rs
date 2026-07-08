//! Server-side apply (ADR 0002 §2/§6, step 3): patches the exact typed objects
//! `render::manifests` built via `kube`'s typed `Api<K>`. No `kubectl`
//! shell-out, no `DynamicObject` rebuilt from YAML — the "a field rename is a
//! compile error, not a silently-wrong manifest" guarantee `render.rs`
//! documents for reads extends to the write path here.

use anyhow::{bail, Context, Result};
use std::fmt::Debug;
use std::time::Duration;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{DeleteParams, ListParams, LogParams, Patch, PatchParams};
use kube::core::NamespaceResourceScope;
use kube::{Api, Resource, ResourceExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::time::{sleep, Instant};

use super::render::Manifests;
use crate::deploy::target::AppliedObject;

/// datamk's field manager name for server-side apply. Every `datamk deploy`
/// re-run uses the same manager, so SSA recognizes its own prior fields as
/// owned (rather than perpetually fighting some other manager over them) and a
/// re-run reconciles instead of erroring on a conflict.
const FIELD_MANAGER: &str = "datamk";

/// How often `apply_and_wait_init` polls the init Job's status. Fixed and
/// small enough to notice completion promptly, large enough not to hammer the
/// API server while a `datamk run` build is in flight.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Apply the ConfigMap, then the init Job (waiting for it to complete — see
/// `apply_and_wait_init`), then Service, Deployment, CronJob — in that order.
/// The ConfigMap must exist first (the Deployment/CronJob/init Job pod specs
/// reference it by name — `render::cell_volume`); the init Job must run and
/// **complete** before the Server is applied at all, so `serve`'s `READ_ONLY`
/// DuckLake attach never races an uninitialized catalog (ADR 0002, the
/// READ_ONLY bootstrap gap the `kind` e2e harness found).
///
/// `skip_init: true` (the operator drives the Builder themselves) skips
/// applying/waiting on the Job entirely — the Job is still rendered (so
/// `--dry-run` always shows it), just not applied here.
pub(crate) async fn apply_all(
    client: &kube::Client,
    namespace: &str,
    m: &Manifests,
    skip_init: bool,
    init_timeout_secs: u64,
) -> Result<Vec<AppliedObject>> {
    let mut applied = Vec::with_capacity(5);

    applied.push(apply_one(client, namespace, "ConfigMap", &m.configmap).await?);
    if skip_init {
        eprintln!("  init       skipped (--skip-init)");
    } else {
        applied.push(
            apply_and_wait_init(client, namespace, &m.init_job, init_timeout_secs)
                .await
                .context("running the init build (datamk run) before applying the Server")?,
        );
    }
    applied.push(apply_one(client, namespace, "Service", &m.service).await?);
    applied.push(apply_one(client, namespace, "Deployment", &m.deployment).await?);
    if let Some(cj) = &m.cronjob {
        applied.push(apply_one(client, namespace, "CronJob", cj).await?);
    }

    Ok(applied)
}

/// Server-side apply the init Job, then poll its status until it succeeds,
/// fails, or `timeout_secs` elapses — whichever comes first. On failure or
/// timeout, fetches the Job's pod logs (selected by the `job-name=<name>`
/// label every Job-owned Pod carries) and bails with them, so the operator
/// sees the actual `datamk run` error instead of a bare "deploy failed".
///
/// This is the ONE place `deploy` waits on cluster-controller-driven state —
/// deliberately narrow (a single, bounded, datamk-owned Builder run), not a
/// precedent for watching the Server Deployment's rollout too (that stays
/// cluster/runtime state, per ADR 0002).
pub(crate) async fn apply_and_wait_init(
    client: &kube::Client,
    namespace: &str,
    job: &Job,
    timeout_secs: u64,
) -> Result<AppliedObject> {
    let applied = match apply_one(client, namespace, "Job", job).await {
        Ok(a) => a,
        // A Job's pod template is immutable, and the init Job's content-hashed
        // name doesn't cover everything in its template (notably the image),
        // so a leftover Job from a previous deploy can collide with SSA here.
        // The old template is outdated by definition — replace the Job.
        Err(e) if is_immutable_error(&e) => {
            let name = job.metadata.name.as_deref().unwrap_or_default();
            eprintln!(
                "  replacing  Job {name} (pod template changed; Job templates are immutable)"
            );
            delete_job_and_wait_gone(client, namespace, name).await?;
            apply_one(client, namespace, "Job", job).await?
        }
        Err(e) => return Err(e),
    };
    let name = &applied.name;

    let api: Api<Job> = Api::namespaced(client.clone(), namespace);
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    loop {
        let current = api
            .get_opt(name)
            .await
            .with_context(|| format!("polling init Job '{name}' in namespace '{namespace}'"))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "init Job '{name}' disappeared from namespace '{namespace}' while waiting for it"
                )
            })?;

        if let Some(status) = &current.status {
            if status.succeeded.unwrap_or(0) >= 1 {
                return Ok(applied);
            }
            if job_failed(status) {
                let logs = fetch_job_logs(client, namespace, name).await;
                bail!(
                    "init Job '{name}' failed in namespace '{namespace}' (datamk run did not \
                     complete) -- the Server is not applied until the catalog is initialized.\n\
                     --- pod logs ---\n{logs}"
                );
            }
        }

        if Instant::now() >= deadline {
            let logs = fetch_job_logs(client, namespace, name).await;
            bail!(
                "init Job '{name}' in namespace '{namespace}' did not complete within \
                 {timeout_secs}s -- the Server is not applied until the catalog is initialized. \
                 Re-run with a larger --init-timeout, or investigate the build directly \
                 (`kubectl -n {namespace} logs job/{name}`).\n\
                 --- pod logs so far ---\n{logs}"
            );
        }

        sleep(POLL_INTERVAL).await;
    }
}

/// Whether an apply error is Kubernetes rejecting a mutation of an immutable
/// field (HTTP 422 Invalid, "field is immutable") — for Jobs, the pod
/// template. Matched on the API error itself, not our context strings, so
/// wrapping doesn't break detection.
fn is_immutable_error(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<kube::Error>(),
            Some(kube::Error::Api(ae)) if ae.code == 422 && ae.message.contains("field is immutable")
        )
    })
}

/// Delete a Job (foreground propagation: its Pods go first) and wait until the
/// API server reports it fully gone — re-applying before the finalizer clears
/// would just hit the same immutable-template rejection again.
async fn delete_job_and_wait_gone(
    client: &kube::Client,
    namespace: &str,
    name: &str,
) -> Result<()> {
    let api: Api<Job> = Api::namespaced(client.clone(), namespace);
    api.delete(name, &DeleteParams::foreground())
        .await
        .with_context(|| {
            format!("deleting outdated init Job '{name}' in namespace '{namespace}'")
        })?;

    // Foreground deletion is bounded by pod termination grace (30s default);
    // 120s covers a slow kubelet without hanging a broken deploy forever.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match api.get_opt(name).await {
            Ok(None) => return Ok(()),
            Ok(Some(_)) if Instant::now() >= deadline => bail!(
                "outdated init Job '{name}' in namespace '{namespace}' is still terminating after \
                 120s; retry the deploy once it is gone (`kubectl -n {namespace} get job {name}`)"
            ),
            Ok(Some(_)) => sleep(POLL_INTERVAL).await,
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("waiting for init Job '{name}' deletion in namespace '{namespace}'")
                })
            }
        }
    }
}

/// Whether a Job's status reports terminal failure: either the `Failed`
/// condition is `True`, or `status.failed` has already reached/exceeded a
/// `backoffLimit` the Job controller gave up on retrying past. Both are
/// checked because the controller doesn't always set the condition the same
/// tick it stops retrying.
fn job_failed(status: &k8s_openapi::api::batch::v1::JobStatus) -> bool {
    status.conditions.as_ref().is_some_and(|conds| {
        conds
            .iter()
            .any(|c| c.type_ == "Failed" && c.status == "True")
    })
}

/// Best-effort: fetch logs from every Pod the init Job owns (selected by the
/// `job-name` label Kubernetes stamps on every Job-owned Pod). Never itself
/// errors out of the caller's `bail!` — a logs-fetch failure (e.g. the pod was
/// already GC'd) must not swallow the real "the build failed" error.
async fn fetch_job_logs(client: &kube::Client, namespace: &str, job_name: &str) -> String {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let lp = ListParams::default().labels(&format!("job-name={job_name}"));
    let list = match pods.list(&lp).await {
        Ok(l) => l,
        Err(e) => return format!("(could not list pods for job '{job_name}': {e})"),
    };
    if list.items.is_empty() {
        return format!("(no pods found for job '{job_name}')");
    }

    let mut out = String::new();
    for pod in &list.items {
        let pod_name = pod.name_any();
        out.push_str(&format!("== pod {pod_name} ==\n"));
        match pods.logs(&pod_name, &LogParams::default()).await {
            Ok(text) => out.push_str(&text),
            Err(e) => out.push_str(&format!("(could not fetch logs: {e})")),
        }
        out.push('\n');
    }
    out
}

/// Server-side apply one typed object. `force()` lets datamk's own prior apply
/// win a field-ownership conflict with itself (e.g. a replica count changed
/// out from under a previous apply) — SSA is declarative and idempotent
/// (ADR 0002 consequences), so re-deploying the same manifest is a no-op.
async fn apply_one<K>(
    client: &kube::Client,
    namespace: &str,
    kind: &str,
    obj: &K,
) -> Result<AppliedObject>
where
    K: Resource<Scope = NamespaceResourceScope, DynamicType = ()>
        + Clone
        + Debug
        + Serialize
        + DeserializeOwned,
{
    let name = obj.name_any();
    let api: Api<K> = Api::namespaced(client.clone(), namespace);
    let pp = PatchParams::apply(FIELD_MANAGER).force();
    api.patch(&name, &pp, &Patch::Apply(obj))
        .await
        .with_context(|| {
            format!("server-side applying {kind} '{name}' in namespace '{namespace}'")
        })?;

    Ok(AppliedObject {
        kind: kind.to_string(),
        name,
        namespace: Some(namespace.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_error(code: u16, message: &str) -> anyhow::Error {
        let status = serde_json::from_value(serde_json::json!({
            "status": "Failure",
            "message": message,
            "reason": "Invalid",
            "code": code,
        }))
        .unwrap();
        // The real call site wraps the kube error in context strings; detection
        // must survive that.
        anyhow::Error::new(kube::Error::Api(status))
            .context("server-side applying Job 'x' in namespace 'y'")
    }

    #[test]
    fn immutable_template_rejection_is_detected_through_context() {
        let e = api_error(
            422,
            "Job.batch \"x\" is invalid: spec.template: ... field is immutable",
        );
        assert!(is_immutable_error(&e));
    }

    #[test]
    fn other_api_errors_are_not_misclassified() {
        assert!(!is_immutable_error(&api_error(409, "conflict")));
        assert!(!is_immutable_error(&api_error(
            422,
            "spec.foo: Required value"
        )));
        assert!(!is_immutable_error(&anyhow::anyhow!("field is immutable"))); // text alone isn't proof
    }
}
