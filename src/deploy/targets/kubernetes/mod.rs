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

use crate::config::{DeployConfig, Target};
use crate::deploy::target::{DeployContext, DeployReport, DeployTarget, Workload, Workloads};
use render::RenderInput;
use schema::KubernetesConfig;

pub struct Kubernetes;

impl DeployTarget for Kubernetes {
    fn supports(&self) -> Workloads {
        Workloads::Both
    }

    /// Issue #8: `serve:` is presence-based, so whether *this* deploy renders
    /// a Server is an overlay fact, not a target capability. Parsed here
    /// (shape-only, no `validate()` — that runs, with its own context, in
    /// `deploy_impl`) so the agnostic pre-flight can gate the Server-only
    /// checks without learning this target's sub-schema.
    fn serves(&self, cfg: &DeployConfig) -> Result<bool> {
        Ok(parse_overlay(cfg)?.serves())
    }

    fn deploy<'a>(
        &'a self,
        ctx: &'a DeployContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<DeployReport>> + 'a>> {
        Box::pin(self.deploy_impl(ctx))
    }
}

fn parse_overlay(cfg: &DeployConfig) -> Result<KubernetesConfig> {
    serde_yaml::from_value(cfg.raw.clone())
        .context("parsing kubernetes topology in the deploy overlay")
}

impl Kubernetes {
    // Async body: schema validation + workload reconciliation are sync/pure;
    // dry-run stays fully cluster-free (renders and returns — no `kube::Client`
    // is ever constructed on that branch, ADR 0002 §2). A real apply builds the
    // client, runs the cluster-side pre-flight, renders with the live
    // `secret_checksum`, then server-side applies.
    async fn deploy_impl<'a>(&'a self, ctx: &'a DeployContext<'a>) -> Result<DeployReport> {
        let k8s = parse_overlay(ctx.cfg)?;
        // Issue #8: before `validate()`, whose generic neither-`serve`-nor-
        // `schedule` message would suggest `schedule:` — wrong for this cell.
        preflight::check_all_bound_cell_has_a_server(&ctx.def.cell, ctx.all_bound, &k8s)?;
        k8s.validate()
            .context("validating kubernetes topology in the deploy overlay")?;
        // Issue #6/#11: refused here, before the `--dry-run` branch below,
        // so both dry-run (which never touches a cluster) and a real apply
        // refuse `schedule:` set on an all-bound cell — a CronJob invoking
        // `datamk run` on a cell with nothing to build, which would
        // crash-loop on every scheduled tick.
        preflight::check_no_schedule_for_an_all_bound_cell(&ctx.def.cell, ctx.all_bound, &k8s)?;

        // Reconcile workloads: the Server only when `serve:` is present
        // (issue #8 — the cell is servable, the agnostic pre-flight
        // guaranteed it for exactly this case); the Builder only when a
        // schedule is set. `!ctx.all_bound` is defense-in-depth, not the
        // primary guard (the check above already refused this combination
        // outright) — belt and suspenders against `Workload::Scheduled` (and
        // the report it feeds) ever claiming a Builder exists for a cell
        // with nothing to build.
        let mut workloads = Vec::with_capacity(2);
        if k8s.serves() {
            workloads.push(Workload::LongLived);
        }
        if k8s.schedule.is_some() && !ctx.all_bound {
            workloads.push(Workload::Scheduled);
        }
        // Printed as `note:` on both branches. Without a Service the ClusterIP
        // line would describe an object that was never rendered.
        let notes = vec![if k8s.serves() {
            "service is ClusterIP — reachable in-cluster only (ADR §7); no external URL"
                .to_string()
        } else {
            "no `serve:` in the deploy overlay — no Deployment and no Service; this cell \
             is built, not served"
                .to_string()
        }];

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
                all_bound: ctx.all_bound,
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
                notes,
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
            all_bound: ctx.all_bound,
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
            notes,
        })
    }
}
