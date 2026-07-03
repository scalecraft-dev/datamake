use crate::config::Target;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "datamk",
    version,
    about = "the cell — reduce time to value for data"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Scaffold a new cell (an implementation project)
    Init(InitArgs),
    /// Execute the transform pipeline, commit a snapshot, auto-verify (the Builder workload)
    Run(RunArgs),
    /// Machine-verify actual output against the declared interface
    Verify(FileArgs),
    /// Pin the current snapshot as the supported contract
    Release(FileArgs),
    /// Deploy the cell as a managed workload on an orchestrator
    Deploy(DeployArgs),
    /// Serve the declared interface as REST + OpenAPI (the Server workload)
    Serve(ServeArgs),
    /// Show the published executions and the LATEST pointer (published-artifact profiles)
    Status(FileArgs),
    /// Roll back the served DATA to an earlier execution by repointing LATEST.
    /// (To roll back a version/code change, use your orchestrator's rollout undo.)
    Rollback(RollbackArgs),

    /// Deprecated alias for `release`; kept for one release.
    #[command(hide = true)]
    Publish(FileArgs),
}

#[derive(Args)]
pub struct RollbackArgs {
    /// Path to the cell definition
    #[arg(short, long, default_value = "cell.yaml")]
    pub file: PathBuf,
    /// Binding profile to use (reads profiles/<name>.yaml)
    #[arg(short, long)]
    pub profile: String,
    /// Execution number to roll back to (default: the one before LATEST)
    #[arg(long)]
    pub execution: Option<u64>,
}

#[derive(Args)]
pub struct RunArgs {
    /// Path to the cell definition
    #[arg(short, long, default_value = "cell.yaml")]
    pub file: PathBuf,
    /// Binding profile to use (reads profiles/<name>.yaml)
    #[arg(short, long, default_value = "local")]
    pub profile: String,
    /// Published-mode compaction window in days (ADR 0004 §10): expire
    /// snapshots older than this (never pinned ones), delete data files
    /// unreferenced for at least this long, and GC superseded catalog
    /// artifacts. 0 disables compaction. Ignored in direct-attach mode.
    #[arg(long, default_value_t = 30)]
    pub retention_days: u64,
}

#[derive(Args)]
pub struct FileArgs {
    /// Path to the cell definition
    #[arg(short, long, default_value = "cell.yaml")]
    pub file: PathBuf,
    /// Binding profile to use (reads profiles/<name>.yaml)
    #[arg(short, long, default_value = "local")]
    pub profile: String,
}

#[derive(Args)]
pub struct DeployArgs {
    /// Path to the cell definition
    #[arg(short, long, default_value = "cell.yaml")]
    pub file: PathBuf,
    /// Binding profile: reads profiles/<name>.yaml + deploy/<name>.yaml. Required —
    /// you don't deploy `local`.
    #[arg(short, long)]
    pub profile: String,
    /// Orchestrator to deploy to (overrides `target:` in deploy/<profile>.yaml)
    #[arg(long, value_enum)]
    pub target: Option<Target>,
    /// Render the target's manifests to stdout without applying
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the deploy-time init build. By default `deploy` runs `datamk run`
    /// once and waits for it, so the Server never starts against an
    /// uninitialized catalog; pass this if you drive the Builder yourself.
    #[arg(long)]
    pub skip_init: bool,
    /// Seconds to wait for the init build to complete before failing the deploy.
    #[arg(long, default_value_t = 300)]
    pub init_timeout: u64,
}

#[derive(Args)]
pub struct InitArgs {
    /// Cell name
    pub name: String,
    /// Directory to create (defaults to ./<name>)
    #[arg(short, long)]
    pub path: Option<PathBuf>,
}

#[derive(Args)]
pub struct ServeArgs {
    /// Path to the cell definition
    #[arg(short, long, default_value = "cell.yaml")]
    pub file: PathBuf,
    /// Binding profile to use (reads profiles/<name>.yaml)
    #[arg(short = 'P', long, default_value = "local")]
    pub profile: String,
    /// Port to bind
    #[arg(short, long, default_value_t = 8080)]
    pub port: u16,
    /// Seconds between LATEST-pointer checks in published-artifact mode — the
    /// staleness bound for experimental "latest" routes (ADR 0004). Ignored in
    /// direct-attach (local catalog) mode.
    #[arg(long, default_value_t = 15)]
    pub poll_interval: u64,
}
