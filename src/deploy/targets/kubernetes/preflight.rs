//! Cluster-side pre-flight (ADR 0002 §6). The agnostic pre-flight
//! (`src/deploy/preflight.rs`, ADR 0001 §7/§8) only checks the profile/cell.yaml
//! *shape* — it has no cluster to ask. This module realizes the Kubernetes-
//! specific half of §6 against the real cluster, as hard failures that block
//! apply: the Secrets a rendered manifest references must actually exist and
//! actually parse before a single object is applied.
//!
//! Split into **pure** checks (repo-local facts — no cluster, no I/O) and
//! **live** checks (the `kube::Client` really asks the API server), so the pure
//! half stays unit-testable without a reachable cluster.

use anyhow::{anyhow, bail, Context, Result};

use k8s_openapi::api::core::v1::Secret;
use kube::Api;

use super::render;
use super::schema::KubernetesConfig;
use crate::config::ResolvedBindings;
use crate::deploy::target::DeployContext;

/// Run every cluster-side check. Returns the principals Secret's
/// `resourceVersion` — `Some` only when `has_roles` (an open cell mounts no
/// principals Secret to version), `None` otherwise — so the caller can stamp
/// it as the Deployment's `checksum/secret` pod-template annotation: rotating
/// the Secret then rolls the Server (ADR 0002 §5).
pub(crate) async fn check(
    client: &kube::Client,
    namespace: &str,
    ctx: &DeployContext<'_>,
    k8s: &KubernetesConfig,
    has_roles: bool,
) -> Result<Option<String>> {
    check_principals_mount_path(ctx.bindings, has_roles)?;

    let secrets: Api<Secret> = Api::namespaced(client.clone(), namespace);

    let profile_name = render::profile_secret_name(&ctx.def.cell, ctx.profile);
    require_secret(
        &secrets,
        namespace,
        &profile_name,
        "the profile Secret (catalog DSN + S3 creds every workload mounts)",
    )
    .await?;

    if let Some(pull_secret) = &k8s.image_pull_secret {
        require_secret(&secrets, namespace, pull_secret, "the `imagePullSecret`").await?;
    }

    if !has_roles {
        return Ok(None);
    }

    let principals_name = render::principals_secret_name(&ctx.def.cell);
    let secret = require_secret(
        &secrets,
        namespace,
        &principals_name,
        "the principals Secret (`access.roles` is set)",
    )
    .await?;
    let resource_version = check_principals_secret(&secret, namespace, &principals_name)?;
    Ok(Some(resource_version))
}

// --- pure checks (no cluster) -----------------------------------------------

// NOTE: the former `check_replica_catalog` (replicas > 1 ⇒ Postgres) is gone —
// under ADR 0004 every replica holds its own private local catalog copy, so
// replica count no longer constrains the catalog at all.

/// Issue #6/#11: the agnostic pre-flight's `check_no_all_never` relax
/// (`src/deploy/preflight.rs`) stops refusing an all-bound cell outright —
/// deploying just the Server is now legitimate. But Kubernetes always
/// reports `Workloads::Both`, so that relax alone would silently let
/// `schedule:` through for an all-bound cell too: a CronJob invoking
/// `datamk run` on a cell with no materializing transforms, which `run`
/// itself already refuses before BEGIN (`engine::run`) — a crash-loop every
/// scheduled tick, not a build failure caught once at deploy time. This is
/// the target-specific half the agnostic layer can't do itself (it has no
/// visibility into `KubernetesConfig.schedule`, which lives in this
/// target's own topology config).
///
/// A pure check (no cluster needed) called directly from `deploy_impl`,
/// before the `--dry-run` branch — not from the async `check` above, which
/// only ever runs on a real apply. `--dry-run` never touches a cluster, but
/// it still renders a CronJob manifest whenever `k8s.schedule` is set
/// (`render_cronjob` reads it directly, independent of the reconciled
/// `workloads` list) — refusing here means `--dry-run` catches this
/// combination too, not just a real apply.
pub(super) fn check_no_schedule_for_an_all_bound_cell(
    cell: &str,
    all_bound: bool,
    k8s: &KubernetesConfig,
) -> Result<()> {
    if all_bound && k8s.schedule.is_some() {
        bail!(
            "cell '{cell}' has no materializing transforms (every export is bound) — \
             `schedule:` would deploy a Builder with nothing to build. `datamk run` already \
             refuses this cell before BEGIN, so the CronJob would crash-loop on every \
             scheduled tick.\n\
             Remove `schedule:` from this deploy overlay — an all-bound cell only needs the \
             Server (`datamk serve`); there is no Builder to run."
        );
    }
    Ok(())
}

/// When `access.roles` is set, the profile's `principals:` must equal the path
/// the principals Secret is actually mounted at in-cluster (ADR 0002 §5) — the
/// only place that Secret's data lands. Any other value means `serve` starts
/// up reading nothing (or the wrong file) and silently denies every request.
fn check_principals_mount_path(bindings: &ResolvedBindings, has_roles: bool) -> Result<()> {
    if !has_roles {
        return Ok(());
    }
    let want = render::principals_mount_path();
    if bindings.principals.as_deref() != Some(want.as_str()) {
        let got = match &bindings.principals {
            Some(p) => format!("`{p}`"),
            None => "nothing".to_string(),
        };
        bail!(
            "cell has `access.roles` set, so the profile's `principals:` must equal the \
             in-cluster mount path `{want}` (that's where the principals Secret lands) — got {got}.\n\
             Set `principals: {want}` in the deploy profile."
        );
    }
    Ok(())
}

// --- live checks (asks the cluster) -----------------------------------------

/// Fetch a Secret by name, bailing with an actionable, named error if it's
/// absent. `deploy` never creates Secrets (ADR 0002 §5) — it only references
/// and validates operator-created ones, so a missing Secret is always the
/// operator's action item, never datamk's.
async fn require_secret(
    api: &Api<Secret>,
    namespace: &str,
    name: &str,
    purpose: &str,
) -> Result<Secret> {
    api.get_opt(name)
        .await
        .with_context(|| format!("checking for Secret '{name}' in namespace '{namespace}'"))?
        .ok_or_else(|| {
            anyhow!(
                "Secret '{name}' not found in namespace '{namespace}' — this is {purpose}.\n\
                 `deploy` only references Secrets, it never creates them; create '{name}' in \
                 '{namespace}' before re-running deploy."
            )
        })
}

/// Validate the principals Secret's shape (ADR 0002 §6): it must carry the
/// `principals.json` key, that key's `ByteString` must be valid UTF-8, and it
/// must parse via the *same* `parse_principals` `serve` uses at startup —
/// `load_principals` swallows malformed JSON into an all-deny map, so a deploy
/// that doesn't check this can pass while the pod it produces silently denies
/// every request. Returns the Secret's `resourceVersion` for the checksum
/// annotation.
fn check_principals_secret(secret: &Secret, namespace: &str, name: &str) -> Result<String> {
    // `Secret.data` is already base64-**decoded** to raw bytes by `kube`
    // (`BTreeMap<String, ByteString>`) — decoding it again here would corrupt it.
    let data = secret.data.as_ref().ok_or_else(|| {
        anyhow!(
            "Secret '{name}' in namespace '{namespace}' has no `data` at all; expected a \
             `{key}` key (ADR 0002 §5).",
            key = render::PRINCIPALS_FILE,
        )
    })?;
    let bytes = data.get(render::PRINCIPALS_FILE).ok_or_else(|| {
        anyhow!(
            "Secret '{name}' in namespace '{namespace}' has no `{key}` key in `data` — that's \
             the key both the in-cluster mount and `serve` expect.",
            key = render::PRINCIPALS_FILE,
        )
    })?;
    let raw = std::str::from_utf8(&bytes.0).with_context(|| {
        format!(
            "Secret '{name}' key `{key}` in namespace '{namespace}' is not valid UTF-8",
            key = render::PRINCIPALS_FILE,
        )
    })?;
    crate::serve::parse_principals(raw).with_context(|| {
        format!(
            "Secret '{name}' key `{key}` in namespace '{namespace}' failed to parse as \
             principals JSON",
            key = render::PRINCIPALS_FILE,
        )
    })?;

    Ok(secret.metadata.resource_version.clone().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn orders(profile: &str) -> crate::config::LoadedCell {
        crate::config::load(Path::new("test/integrations/orders/cell.yaml"), profile).unwrap()
    }

    #[test]
    fn schedule_on_an_all_bound_cell_is_refused() {
        let k8s: KubernetesConfig = serde_yaml::from_str("schedule: \"0 * * * *\"").unwrap();
        let err = check_no_schedule_for_an_all_bound_cell("t", true, &k8s)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cell 't'"), "got: {err}");
        assert!(err.contains("no materializing transforms"), "got: {err}");
        assert!(err.contains("crash-loop"), "got: {err}");
        assert!(err.contains("Remove `schedule:`"), "got: {err}");
    }

    #[test]
    fn schedule_on_a_materializing_cell_passes() {
        let k8s: KubernetesConfig = serde_yaml::from_str("schedule: \"0 * * * *\"").unwrap();
        check_no_schedule_for_an_all_bound_cell("t", false, &k8s).unwrap();
    }

    #[test]
    fn an_all_bound_cell_with_no_schedule_passes() {
        let k8s = KubernetesConfig::default();
        check_no_schedule_for_an_all_bound_cell("t", true, &k8s).unwrap();
    }

    #[test]
    fn roles_with_no_principals_path_is_refused() {
        let l = orders("prod"); // profiles/prod.yaml sets no `principals:`
        let err = check_principals_mount_path(&l.bindings, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("in-cluster mount path"), "got: {err}");
        assert!(err.contains("got nothing"), "got: {err}");
    }

    #[test]
    fn roles_with_a_mismatched_principals_path_is_refused() {
        let l = orders("prod");
        let mut bindings = l.bindings.clone();
        bindings.principals = Some("/not/the/mount/path.json".to_string());
        let err = check_principals_mount_path(&bindings, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("/not/the/mount/path.json"), "got: {err}");
        assert!(err.contains(&render::principals_mount_path()), "got: {err}");
    }

    #[test]
    fn roles_with_the_matching_mount_path_passes() {
        let l = orders("prod");
        let mut bindings = l.bindings.clone();
        bindings.principals = Some(render::principals_mount_path());
        check_principals_mount_path(&bindings, true).unwrap();
    }

    #[test]
    fn no_roles_never_checks_the_principals_path() {
        // has_roles: false skips the check outright, whatever `principals:` is.
        let l = orders("local"); // no `principals:` set either
        check_principals_mount_path(&l.bindings, false).unwrap();
    }
}
