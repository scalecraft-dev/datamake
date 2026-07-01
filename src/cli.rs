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
    Run(FileArgs),
    /// Machine-verify actual output against the declared interface
    Verify(FileArgs),
    /// Pin the current snapshot as the supported contract
    Release(FileArgs),
    /// Deploy the cell as a managed workload on an orchestrator
    Deploy(DeployArgs),
    /// Serve the declared interface as REST + OpenAPI (the Server workload)
    Serve(ServeArgs),

    /// Deprecated alias for `release`; kept for one release.
    #[command(hide = true)]
    Publish(FileArgs),
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
}
