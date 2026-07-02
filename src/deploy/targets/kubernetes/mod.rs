//! Kubernetes target (ADR 0002). Behind the `kubernetes` cargo feature.
//!
//! `--dry-run` renders the real manifests (`render::manifests`) to stdout and
//! touches no cluster — the same path CI can use to validate template
//! correctness. A real apply connects to the cluster (`kube::Client`), runs
//! the cluster-side pre-flight (`preflight::check`, ADR 0002 §6), and
//! server-side applies the same typed objects (`apply::apply_all`, ADR 0002 §2).

mod apply;
mod preflight;
mod render;
mod schema;

use anyhow::{Context, Result};
use std::future::Future;
use std::pin::Pin;

use crate::config::Target;
use crate::deploy::target::{DeployContext, DeployReport, DeployTarget, Workload, Workloads};
use render::RenderInput;
use schema::KubernetesConfig;

pub struct Kubernetes;

impl DeployTarget for Kubernetes {
    fn supports(&self) -> Workloads {
        Workloads::Both
    }

    fn deploy<'a>(
        &'a self,
        ctx: &'a DeployContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<DeployReport>> + 'a>> {
        Box::pin(self.deploy_impl(ctx))
    }
}

impl Kubernetes {
    // Async body: schema validation + workload reconciliation are sync/pure;
    // dry-run stays fully cluster-free (renders and returns — no `kube::Client`
    // is ever constructed on that branch, ADR 0002 §2). A real apply builds the
    // client, runs the cluster-side pre-flight, renders with the live
    // `secret_checksum`, then server-side applies.
    async fn deploy_impl<'a>(&'a self, ctx: &'a DeployContext<'a>) -> Result<DeployReport> {
        let k8s: KubernetesConfig = serde_yaml::from_value(ctx.cfg.raw.clone())
            .context("parsing kubernetes topology in the deploy overlay")?;
        k8s.validate()
            .context("validating kubernetes topology in the deploy overlay")?;

        // Reconcile workloads: the Server always (the cell is servable —
        // pre-flight guaranteed it); the Builder only when a schedule is set.
        let mut workloads = vec![Workload::LongLived];
        if k8s.schedule.is_some() {
            workloads.push(Workload::Scheduled);
        }

        let has_roles = !ctx.def.access.roles.is_empty();
        let namespace = k8s.namespace().to_string();

        if ctx.dry_run {
            // No cluster contact whatsoever: no checksum to ask a Secret for,
            // so the checksum annotation is simply absent (as it always has
            // been on this path).
            let input = RenderInput {
                cell: &ctx.def.cell,
                profile: ctx.profile,
                k8s: &k8s,
                artifact: ctx.artifact,
                has_roles,
                secret_checksum: None,
            };
            let rendered = render::manifests(&input)
                .context("rendering kubernetes manifests")?
                .docs()
                .context("serializing kubernetes manifests")?;

            return Ok(DeployReport {
                target: Target::Kubernetes,
                dry_run: true,
                workloads,
                rendered,
                applied: Vec::new(),
                notes: vec![
                    "service is ClusterIP — reachable in-cluster only (ADR §7); no external URL"
                        .to_string(),
                ],
            });
        }

        let client = kube::Client::try_default()
            .await
            .context("connecting to the Kubernetes cluster (in-cluster config or kubeconfig)")?;

        let secret_checksum = preflight::check(&client, &namespace, ctx, &k8s, has_roles)
            .await
            .with_context(|| format!("kubernetes pre-flight failed (namespace '{namespace}')"))?;

        let input = RenderInput {
            cell: &ctx.def.cell,
            profile: ctx.profile,
            k8s: &k8s,
            artifact: ctx.artifact,
            has_roles,
            secret_checksum: secret_checksum.as_deref(),
        };
        let m = render::manifests(&input).context("rendering kubernetes manifests")?;
        let rendered = m.docs().context("serializing kubernetes manifests")?;

        let applied = apply::apply_all(
            &client,
            &namespace,
            &m,
            ctx.skip_init,
            ctx.init_timeout_secs,
        )
        .await
        .context("applying kubernetes manifests")?;

        Ok(DeployReport {
            target: Target::Kubernetes,
            dry_run: false,
            workloads,
            rendered,
            applied,
            notes: vec![
                "service is ClusterIP — reachable in-cluster only (ADR §7); no external URL"
                    .to_string(),
            ],
        })
    }
}
