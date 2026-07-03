//! Target-agnostic deploy: load the cell (no DB), read the tracked overlay,
//! resolve the target, run pre-flight, then hand a definition + artifact bundle
//! to the backend. `Connection::open_in_memory` appears nowhere on this path.

// With no target feature compiled, the seam types have no consumer and read as
// dead. The default build (kubernetes on) keeps the strict lint, so real dead
// code is still caught there.
#![cfg_attr(not(feature = "kubernetes"), allow(dead_code))]

pub mod artifact;
mod preflight;
pub mod target;
mod targets;

use anyhow::{anyhow, bail, Context, Result};

use crate::cli::DeployArgs;
use crate::config::{self, DeployConfig, Target};
use artifact::CellArtifact;
use preflight::PreflightInput;
use target::{DeployContext, DeployTarget};

pub async fn run(args: &DeployArgs) -> Result<()> {
    // `local` is the run/serve profile (./.cell paths); there is intentionally no
    // deploy/local.yaml. Refuse early with the real reason, not "create the overlay".
    if args.profile == "local" {
        bail!(
            "profile 'local' is not deployable: it uses ./.cell paths for run/serve.\n\
             Deploy a profile backed by a shared object store (e.g. -p prod)."
        );
    }

    // Pure parse + resolve (no database).
    let loaded = config::load(&args.file, &args.profile)?;
    let cfg = DeployConfig::load(&loaded.dir, &args.profile)?;

    // --target overrides the overlay's `target:`; absence of both is a hard error.
    let target_kind = args.target.or(cfg.target).ok_or_else(|| {
        anyhow!(
            "no deploy target: deploy/{p}.yaml has no `target:` and --target was not passed.\n\
             Set `target: kubernetes` in deploy/{p}.yaml, or pass `--target kubernetes`.",
            p = args.profile
        )
    })?;
    let target = build_target(target_kind)?;

    preflight::check(&PreflightInput {
        def: &loaded.def,
        bindings: &loaded.bindings,
        supports: target.supports(),
        allow_anonymous: cfg.allow_anonymous,
        profile: &args.profile,
    })
    .with_context(|| format!("deploy pre-flight failed (profile '{}')", args.profile))?;

    // ADR 0004 §3: the single-writer guard is the store's conditional PUT.
    // The host-side probe is best-effort: the deploy host may legitimately be
    // unable to reach storage that pods can (in-cluster MinIO, private
    // endpoints), so unreachability DEFERS to the authoritative in-pod probe
    // (`engine::run` runs it first thing; the init Job surfaces a failure
    // with the build pod's logs). Only a store that *answers* and proves
    // non-enforcement fails the deploy here. Skipped on --dry-run.
    if !args.dry_run {
        let probe =
            crate::store::Store::for_storage(&loaded.bindings.storage, loaded.bindings.s3.as_ref())
                .and_then(|store| store.probe_conditional_put());
        match probe {
            Ok(crate::store::ProbeOutcome::Enforced) => {
                tracing::info!("conditional-PUT capability probe passed");
            }
            Ok(crate::store::ProbeOutcome::NotEnforced) => {
                bail!(
                    "deploy pre-flight failed (profile '{}'): {}",
                    args.profile,
                    crate::store::NOT_ENFORCED_MSG
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "storage not probeable from the deploy host; the conditional-PUT probe \
                     runs in-cluster during the init build instead"
                );
            }
        }
    }

    // Gather deliverable content (pure I/O; profile excluded — it's secret-grade).
    let cell_yaml_rel = args
        .file
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("cell.yaml");
    let artifact = CellArtifact::collect(&loaded.dir, cell_yaml_rel, &loaded.def)?;

    let ctx = DeployContext {
        def: &loaded.def,
        artifact: &artifact,
        bindings: &loaded.bindings,
        cfg: &cfg,
        profile: &args.profile,
        dry_run: args.dry_run,
        skip_init: args.skip_init,
        init_timeout_secs: args.init_timeout,
    };
    let report = target.deploy(&ctx).await?;
    report.print(&loaded.def.cell, &args.profile);
    Ok(())
}

/// Resolve a target kind to its backend. Targets are feature-gated: a build
/// without the `kubernetes` feature still compiles `deploy`, it just has no
/// backend to offer.
fn build_target(kind: Target) -> Result<Box<dyn DeployTarget>> {
    match kind {
        #[cfg(feature = "kubernetes")]
        Target::Kubernetes => Ok(Box::new(targets::kubernetes::Kubernetes)),
        #[cfg(not(feature = "kubernetes"))]
        Target::Kubernetes => {
            anyhow::bail!("datamk was built without the `kubernetes` feature")
        }
    }
}
