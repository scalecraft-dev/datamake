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
    /// Promote the current snapshot to the supported contract
    Publish(FileArgs),
    /// Serve the declared interface as REST + OpenAPI (the Server workload)
    Serve(ServeArgs),
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
