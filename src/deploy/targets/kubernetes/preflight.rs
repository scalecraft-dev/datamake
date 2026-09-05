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
use crate::config::{Bindings, ResolvedBindings};
use crate::deploy::target::DeployContext;

/// Run every cluster-side check. Returns the principals Secret's
/// `resourceVersion` — `Some` only when the Server is rendered *and*
/// `has_roles` (an open cell mounts no principals Secret to version; a
/// Builder-only deploy mounts none at all), `None` otherwise — so the caller
/// can stamp it as the Deployment's `checksum/secret` pod-template
/// annotation: rotating the Secret then rolls the Server (ADR 0002 §5).
///
/// Every Secret is required only where a rendered pod actually mounts it
/// (issue #14): the Builder profile when a Builder exists (not all-bound),
/// the Server profile + principals only when `serve:` is present.
pub(crate) async fn check(
    client: &kube::Client,
    namespace: &str,
    ctx: &DeployContext<'_>,
    k8s: &KubernetesConfig,
    has_roles: bool,
) -> Result<Option<String>> {
    let server_has_roles = has_roles && k8s.serves();
    check_principals_mount_path(ctx.bindings, server_has_roles)?;

    let secrets: Api<Secret> = Api::namespaced(client.clone(), namespace);

    if !ctx.all_bound {
        let profile_name = render::profile_secret_name(&ctx.def.cell, ctx.profile);
        require_secret(
            &secrets,
            namespace,
            &profile_name,
            "the Builder's profile Secret (the full profile — storage creds + warehouse \
             connections; mounted only into Builder pods)",
        )
        .await?;
    }

    if k8s.serves() {
        let server_profile_name = render::server_profile_secret_name(&ctx.def.cell, ctx.profile);
        let secret = require_secret(
            &secrets,
            namespace,
            &server_profile_name,
            "the Server's profile Secret (the reduced profile — storage creds + `principals:`, \
             NO `connections:`; the only profile the Server pod mounts)",
        )
        .await?;
        check_server_profile_secret(&secret, namespace, &server_profile_name, ctx.profile)?;
    }

    if let Some(pull_secret) = &k8s.image_pull_secret {
        require_secret(&secrets, namespace, pull_secret, "the `imagePullSecret`").await?;
    }

    if !server_has_roles {
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

/// Issue #8 × issue #6/#11: with `serve:` now presence-based, an all-bound
/// cell that omits it has no workload left — the Builder is already refused
/// for it (above), and there is no Server to render. `KubernetesConfig::
/// validate`'s generic neither-`serve`-nor-`schedule` message would tell the
/// author to add `schedule:`, which is exactly the wrong advice for this
/// cell, so this runs **before** `validate()` in `deploy_impl` and names the
/// only fix that works.
pub(super) fn check_all_bound_cell_has_a_server(
    cell: &str,
    all_bound: bool,
    k8s: &KubernetesConfig,
) -> Result<()> {
    if all_bound && !k8s.serves() {
        let and_schedule = if k8s.schedule.is_some() {
            " and remove `schedule:` (a Builder has nothing to build here)"
        } else {
            ""
        };
        bail!(
            "cell '{cell}' has no materializing transforms (every export is bound), so the \
             Server (`datamk serve`, which serves `/context`) is the only workload it can \
             run — and this deploy overlay has no `serve:` block, so nothing would be \
             deployed.\n\
             Add `serve: {{}}` to this deploy overlay{and_schedule}."
        );
    }
    Ok(())
}

/// Issue #14 × issue #6/#11: `serviceAccounts.builder` on an all-bound cell
/// names an account no pod would ever run as — no init Job, no CronJob
/// (`render_init_job`/`render_cronjob` are both `None`). The Server-side
/// mismatch is refused in `KubernetesConfig::validate` (it only needs
/// `serve:`); this one needs `all_bound`, so it lives here beside
/// `check_no_schedule_for_an_all_bound_cell`.
pub(super) fn check_builder_service_account_for_an_all_bound_cell(
    cell: &str,
    all_bound: bool,
    k8s: &KubernetesConfig,
) -> Result<()> {
    if let (true, Some(name)) = (all_bound, &k8s.service_accounts.builder) {
        bail!(
            "cell '{cell}' has no materializing transforms (every export is bound), so no \
             Builder workload is rendered — `serviceAccounts.builder: {name}` would never be \
             assigned to anything.\n\
             Remove `serviceAccounts.builder` from this deploy overlay; only the Server has an \
             identity to assign for this cell."
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

/// Issue #14: the invariant the Secret split exists for — **no connector
/// DSN reaches the Server pod** — is only half provable from the render (the
/// Deployment mounts `<cell>-<profile>-server`, pinned by a render test).
/// The other half is operator-authored content, and this is the only place
/// it can be checked before a pod exists: the Server Secret's
/// `<profile>.yaml` must parse as a profile and carry no `connections:`.
/// Parsed with the same `Bindings` shape `config::load` uses, so a typo'd
/// key fails here with serde's message rather than in a crash-looping pod.
fn check_server_profile_secret(
    secret: &Secret,
    namespace: &str,
    name: &str,
    profile: &str,
) -> Result<()> {
    let key = format!("{profile}.yaml");
    let data = secret.data.as_ref().ok_or_else(|| {
        anyhow!(
            "Secret '{name}' in namespace '{namespace}' has no `data` at all; expected a \
             `{key}` key (ADR 0002 §5)."
        )
    })?;
    let bytes = data.get(&key).ok_or_else(|| {
        anyhow!(
            "Secret '{name}' in namespace '{namespace}' has no `{key}` key in `data` — that's \
             the key both the in-cluster mount and `serve --profile {profile}` expect."
        )
    })?;
    let raw = std::str::from_utf8(&bytes.0).with_context(|| {
        format!("Secret '{name}' key `{key}` in namespace '{namespace}' is not valid UTF-8")
    })?;
    let bindings: Bindings = serde_yaml::from_str(raw).with_context(|| {
        format!(
            "Secret '{name}' key `{key}` in namespace '{namespace}' failed to parse as a \
             profile (profiles/{profile}.yaml shape)"
        )
    })?;
    if !bindings.connections.is_empty() {
        let names: Vec<_> = bindings.connections.keys().cloned().collect();
        bail!(
            "Secret '{name}' key `{key}` in namespace '{namespace}' carries `connections:` \
             ({}) — the Server never reads a warehouse connection, and this Secret is the one \
             the Server pod mounts, so warehouse coordinates must not be in it.\n\
             Create '{name}' from a copy of profiles/{profile}.yaml with the `connections:` \
             block removed; the full profile belongs only in the Builder's Secret \
             ('{builder}').",
            names.join(", "),
            builder = name.trim_end_matches("-server"),
        );
    }
    Ok(())
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

    /// Issue #8: an all-bound cell with no `serve:` has no workload at all,
    /// and the fix is `serve: {}` — never `schedule:`.
    #[test]
    fn all_bound_cell_without_serve_is_refused_with_the_serve_fix() {
        let k8s: KubernetesConfig = serde_yaml::from_str("namespace: data").unwrap();
        let err = check_all_bound_cell_has_a_server("t", true, &k8s)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cell 't'"), "got: {err}");
        assert!(err.contains("Add `serve: {}`"), "got: {err}");
        assert!(!err.contains("remove `schedule:`"), "got: {err}");

        let with_schedule: KubernetesConfig =
            serde_yaml::from_str("schedule: \"0 * * * *\"").unwrap();
        let err = check_all_bound_cell_has_a_server("t", true, &with_schedule)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Add `serve: {}`"), "got: {err}");
        assert!(err.contains("remove `schedule:`"), "got: {err}");
    }

    #[test]
    fn all_bound_cell_with_serve_passes_and_materializing_cell_never_needs_one() {
        let serving: KubernetesConfig = serde_yaml::from_str("serve: {}").unwrap();
        check_all_bound_cell_has_a_server("t", true, &serving).unwrap();
        let builder_only: KubernetesConfig =
            serde_yaml::from_str("schedule: \"0 * * * *\"").unwrap();
        check_all_bound_cell_has_a_server("t", false, &builder_only).unwrap();
    }

    /// Issue #14: a Builder account on a cell that renders no Builder.
    #[test]
    fn builder_service_account_on_an_all_bound_cell_is_refused() {
        let k8s: KubernetesConfig =
            serde_yaml::from_str("serve: {}\nserviceAccounts:\n  builder: orders-builder\n")
                .unwrap();
        let err = check_builder_service_account_for_an_all_bound_cell("t", true, &k8s)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cell 't'"), "got: {err}");
        assert!(
            err.contains("serviceAccounts.builder: orders-builder"),
            "got: {err}"
        );
        assert!(
            err.contains("Remove `serviceAccounts.builder`"),
            "got: {err}"
        );

        check_builder_service_account_for_an_all_bound_cell("t", false, &k8s).unwrap();
        let no_sa: KubernetesConfig = serde_yaml::from_str("serve: {}").unwrap();
        check_builder_service_account_for_an_all_bound_cell("t", true, &no_sa).unwrap();
    }

    fn secret_with(profile: &str, yaml: &str) -> Secret {
        let mut data = std::collections::BTreeMap::new();
        data.insert(
            format!("{profile}.yaml"),
            k8s_openapi::ByteString(yaml.as_bytes().to_vec()),
        );
        Secret {
            data: Some(data),
            ..Default::default()
        }
    }

    /// Issue #14: the content half of "no connector DSN reaches the Server".
    #[test]
    fn server_profile_secret_with_connections_is_refused() {
        let secret = secret_with(
            "prod",
            "storage: s3://b/cells/orders\nconnections:\n  wh:\n    type: postgres\n    host: db.internal\n    database: crm\n    user: u\n    password: ${PW}\n",
        );
        let err = check_server_profile_secret(&secret, "data", "orders-prod-server", "prod")
            .unwrap_err()
            .to_string();
        assert!(err.contains("carries `connections:` (wh)"), "got: {err}");
        assert!(err.contains("'orders-prod'"), "got: {err}");
        assert!(err.contains("Server pod mounts"), "got: {err}");
    }

    #[test]
    fn server_profile_secret_without_connections_passes() {
        let secret = secret_with(
            "prod",
            "storage: s3://b/cells/orders\ns3:\n  region: us-east-1\nprincipals: /etc/datamk/principals.json\n",
        );
        check_server_profile_secret(&secret, "data", "orders-prod-server", "prod").unwrap();
    }

    #[test]
    fn server_profile_secret_wrong_key_or_shape_is_refused() {
        let wrong_key = secret_with("staging", "storage: s3://b\n");
        let err = check_server_profile_secret(&wrong_key, "data", "orders-prod-server", "prod")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no `prod.yaml` key"), "got: {err}");

        let typo = secret_with("prod", "storage: s3://b\nconection: {}\n");
        let err = check_server_profile_secret(&typo, "data", "orders-prod-server", "prod")
            .unwrap_err()
            .to_string();
        assert!(err.contains("failed to parse as a profile"), "got: {err}");
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
