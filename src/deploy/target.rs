use anyhow::Result;

use crate::config::{CellDef, DeployConfig, ResolvedBindings, Target};
use crate::deploy::artifact::CellArtifact;

/// A cell's two runtime halves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    /// The Builder (`datamk run`) — runs on a schedule to rebuild snapshots.
    Scheduled,
    /// The Server (`datamk serve`) — runs continuously to expose the interface.
    LongLived,
}

/// Which of the cell's workloads a target can host. Only `Both` is produced in
/// ADR 0001 (the Kubernetes target); `Scheduled`/`LongLived` model scheduler-only
/// (Airflow/Dagster) or server-only targets that arrive in later ADRs.
#[allow(dead_code)] // Scheduled/LongLived realized by future targets (ADR 0002+)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workloads {
    Scheduled,
    LongLived,
    Both,
}

impl Workloads {
    pub fn supports(self, w: Workload) -> bool {
        matches!(
            (self, w),
            (Workloads::Both, _)
                | (Workloads::Scheduled, Workload::Scheduled)
                | (Workloads::LongLived, Workload::LongLived)
        )
    }

    /// Whether this target can host the long-lived Server. Gates the serve/auth
    /// pre-flight: a scheduler-only target skips those checks.
    pub fn long_lived(self) -> bool {
        self.supports(Workload::LongLived)
    }
}

/// Everything a target needs to render/apply a cell: the definition + artifact
/// bundle + resolved bindings (env referenced, not embedded) + the deploy config.
/// Deliberately **not** a live `engine::Cell` — deploy must never attach a database.
pub struct DeployContext<'a> {
    pub def: &'a CellDef,
    pub artifact: &'a CellArtifact,
    pub bindings: &'a ResolvedBindings,
    pub cfg: &'a DeployConfig,
    pub dry_run: bool,
}

/// One rendered document (e.g. a Kubernetes manifest in ADR 0002). In ADR 0001
/// the stub target emits a single human-readable plan doc.
#[derive(Debug, Clone)]
pub struct RenderedDoc {
    pub kind: String,
    pub name: String,
    pub body: String,
}

/// One object actually applied to the orchestrator. Populated only by a real
/// apply, which lands in ADR 0002; empty on every ADR 0001 path.
#[allow(dead_code)] // produced by real apply (ADR 0002)
#[derive(Debug, Clone)]
pub struct AppliedObject {
    pub kind: String,
    pub name: String,
    pub namespace: Option<String>,
}

/// The outcome of a deploy. `rendered` is always populated (it is the only thing
/// `--dry-run` prints); `applied` is empty on a dry run.
pub struct DeployReport {
    pub target: Target,
    pub dry_run: bool,
    pub workloads: Vec<Workload>,
    pub rendered: Vec<RenderedDoc>,
    pub applied: Vec<AppliedObject>,
    pub notes: Vec<String>,
}

impl DeployReport {
    /// Print the report: a human summary to **stderr**, rendered docs to
    /// **stdout** (so `datamk deploy --dry-run | kubectl apply -f -` works once
    /// real manifests land in ADR 0002).
    pub fn print(&self, cell: &str, profile: &str) {
        let mode = if self.dry_run { " (dry run)" } else { "" };
        eprintln!(
            "deploy{mode} — cell '{cell}' · profile '{profile}' · target {}",
            target_name(self.target)
        );
        for w in &self.workloads {
            let (name, kind, cmd) = match w {
                Workload::Scheduled => ("builder", "scheduled", "datamk run"),
                Workload::LongLived => ("server", "long-lived", "datamk serve"),
            };
            eprintln!("  {name:<8} {kind:<11} ({cmd})");
        }
        eprintln!("  preflight  ok");
        for d in &self.rendered {
            eprintln!("  rendered   {} {}", d.kind, d.name);
        }
        for n in &self.notes {
            eprintln!("  note: {n}");
        }
        if self.dry_run {
            eprintln!("  (dry run — nothing applied)");
        } else {
            for a in &self.applied {
                eprintln!("  applied    {} {}", a.kind, a.name);
            }
        }
        for d in &self.rendered {
            println!("{}", d.body);
        }
    }
}

fn target_name(t: Target) -> &'static str {
    match t {
        Target::Kubernetes => "kubernetes",
    }
}

/// A deploy backend. Orchestrators are additive: a new target is a compile-time
/// feature + a trait impl, not a change to the `deploy` command.
pub trait DeployTarget {
    /// Which of the cell's workloads this target can host.
    fn supports(&self) -> Workloads;

    /// Render + (unless `ctx.dry_run`) apply whatever runs the cell here.
    fn deploy(&self, ctx: &DeployContext) -> Result<DeployReport>;
}
