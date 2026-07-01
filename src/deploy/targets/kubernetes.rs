//! Kubernetes target — **stub** for ADR 0001.
//!
//! In ADR 0001 this validates the overlay's Kubernetes topology schema, reconciles
//! which workloads would be deployed, and produces a structured dry-run plan — all
//! with no cluster deps (`kube`/`k8s-openapi` arrive on this same feature in ADR
//! 0002, which replaces the plan with real manifest rendering + a server-side
//! apply). A real apply is refused here, pointing at ADR 0002.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fmt::Write as _;

use crate::config::Target;
use crate::deploy::target::{
    DeployContext, DeployReport, DeployTarget, RenderedDoc, Workload, Workloads,
};

pub struct Kubernetes;

/// The Kubernetes-specific topology, deserialized from the deploy overlay. The
/// authoritative schema is ADR 0002; this captures the sketched fields so the
/// stub can validate types and summarize the plan. Unknown keys (`target`,
/// `allow_anonymous`, future fields) are ignored by serde.
#[derive(Debug, Default, Deserialize)]
struct KubernetesConfig {
    #[serde(default)]
    namespace: Option<String>,
    /// Builder cron. Absent ⇒ no scheduled Builder is deployed.
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    serve: ServeTopology,
}

#[derive(Debug, Default, Deserialize)]
struct ServeTopology {
    #[serde(default)]
    replicas: Option<u32>,
}

impl DeployTarget for Kubernetes {
    fn supports(&self) -> Workloads {
        Workloads::Both
    }

    fn deploy(&self, ctx: &DeployContext) -> Result<DeployReport> {
        // Validate the topology schema (real, tested) — a typo'd `replicas` or a
        // non-string `schedule` is caught here.
        let k8s: KubernetesConfig = serde_yaml::from_value(ctx.cfg.raw.clone())
            .context("parsing kubernetes topology in the deploy overlay")?;

        // Reconcile workloads: the Server always (the cell is servable — pre-flight
        // guaranteed it); the Builder only when a schedule is configured.
        let mut workloads = vec![Workload::LongLived];
        if k8s.schedule.is_some() {
            workloads.push(Workload::Scheduled);
        }

        if !ctx.dry_run {
            bail!(
                "kubernetes apply is not yet implemented (ADR 0002); \
                 re-run with --dry-run to review the plan"
            );
        }

        let plan = render_plan(ctx, &k8s, &workloads);
        Ok(DeployReport {
            target: Target::Kubernetes,
            dry_run: ctx.dry_run,
            workloads,
            rendered: vec![RenderedDoc {
                kind: "plan".to_string(),
                name: ctx.def.cell.clone(),
                body: plan,
            }],
            applied: Vec::new(),
            notes: vec!["manifest rendering and apply land in ADR 0002".to_string()],
        })
    }
}

fn render_plan(ctx: &DeployContext, k8s: &KubernetesConfig, workloads: &[Workload]) -> String {
    let art = ctx.artifact;
    let mut s = String::new();
    let _ = writeln!(s, "# kubernetes deploy plan for cell '{}'", ctx.def.cell);
    let _ = writeln!(
        s,
        "# (ADR 0001 stub — no manifests rendered yet; see ADR 0002)"
    );
    let _ = writeln!(
        s,
        "namespace: {}",
        k8s.namespace.as_deref().unwrap_or("default")
    );
    let _ = writeln!(
        s,
        "image: {}",
        k8s.image.as_deref().unwrap_or("ghcr.io/scalecraft/datamk")
    );
    let _ = writeln!(s, "catalog: {}", ctx.bindings.catalog);
    let _ = writeln!(s, "storage: {}", ctx.bindings.storage);
    let _ = writeln!(s, "workloads:");
    for w in workloads {
        match w {
            Workload::LongLived => {
                let replicas = k8s.serve.replicas.unwrap_or(1);
                let _ = writeln!(s, "  - server (Deployment, replicas: {replicas})");
            }
            Workload::Scheduled => {
                let _ = writeln!(
                    s,
                    "  - builder (CronJob, schedule: {})",
                    k8s.schedule.as_deref().unwrap_or("")
                );
            }
        }
    }
    let _ = writeln!(s, "artifact:");
    let _ = writeln!(s, "  source: {}", art.dir.display());
    let _ = writeln!(s, "  content_hash: {}", art.content_hash);
    let _ = writeln!(
        s,
        "  files: {} ({})",
        art.sql.len() + 1,
        art.cell_yaml.rel_path
    );
    let _ = writeln!(
        s,
        "  pin: {}",
        if art.published.is_some() {
            ".cell/published.json (supported routes frozen)"
        } else {
            "none (run `datamk release` to pin supported routes)"
        }
    );
    s
}
